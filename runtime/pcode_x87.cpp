#include "pcode_x87.h"

#include <cmath>

bool pcode_x87_supported(uint64_t id, const RpOp &op) {
    return (id >> 32) == 1 && (id & 255) >= 1 && (id & 255) <= 15 && op.input_count >= 1 &&
           op.input_count <= 2 && op.inputs[0].offset == id;
}

bool pcode_x87_execute(PcodeHost &host, X86 *c, const RpOp &op, const uint64_t *args,
                       uint64_t &result, std::string &why) {
    unsigned kind = op.userop & 255, a = (op.userop >> 8) & 255;
    unsigned bits = (op.userop >> 16) & 255, flags = (op.userop >> 24) & 15;
    unsigned extra = (op.userop >> 28) & 15;
    bool memory = (kind == 1 && a == 8) || kind == 2 || kind == 15 || (kind == 3 && a == 8) ||
                  ((kind == 4 || kind == 5) && bits);
    uint32_t addr = memory ? (uint32_t)args[0] : 0;
    if (memory && (op.input_count != 2 || args[0] > UINT32_MAX ||
                   !host.memory_supported(args[0], bits / 8, kind == 3 || kind == 15, why))) {
        if (why.empty())
            why = "invalid x87 memory operand";
        return false;
    }
    auto integer = [&]() -> int64_t {
        if (bits == 16)
            return (int16_t)rd16(addr);
        if (bits == 32)
            return (int32_t)rd32(addr);
        return (int64_t)rd64(addr);
    };
    auto real = [&]() -> double {
        if (bits == 32)
            return rdf32(addr);
        if (bits == 64)
            return rdf64(addr);
        return rdf80(addr);
    };
    switch (kind) {
    case 1:
        if (a == 8)
            fpush(c, real());
        else
            fpush_st(c, a);
        break;
    case 2:
        fpush_int(c, integer());
        break;
    case 3:
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
        break;
    case 4: {
        unsigned dst = a & 15, src = a >> 4;
        double lhs = ST(c, dst),
               rhs = bits ? ((flags & 8) ? (double)integer() : real()) : ST(c, src);
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
        break;
    }
    case 5: {
        double rhs = (flags & 4) ? 0.0
                     : bits      ? ((flags & 2) ? (double)integer() : real())
                                 : ST(c, a);
        if (flags & 1)
            fucom(c, ST(c, 0), rhs);
        else
            fcom(c, ST(c, 0), rhs);
        for (unsigned i = 0; i < extra; ++i)
            fdrop(c);
        break;
    }
    case 6:
        result = fcomi_flags_sw(&c->fpu_sw, ST(c, 0), ST(c, a), flags);
        if (extra)
            fdrop(c);
        break;
    case 7:
        fxch(c, a);
        break;
    case 8:
        ftag_put(c, (c->fpu_top + a) & 7u, FTAG_EMPTY);
        break;
    case 9:
        result = fstsw(c);
        break;
    case 10:
        x87_finit(c);
        break;
    case 11:
        c->fpu_top = (c->fpu_top + (a ? 1u : 7u)) & 7u;
        break;
    case 12:
        break;
    case 13: {
        double v = ST(c, 0);
        if (a == 0)
            v = fabs(v);
        else if (a == 1)
            v = -v;
        else if (a == 2)
            v = fx87(c, sqrt(v));
        else if (a == 3)
            v = fx87_exact(c, fround_cw(c, v));
        else if (a == 4)
            v = fx87_exact(c, sin(v));
        else if (a == 5)
            v = fx87_exact(c, cos(v));
        else
            v = fx87_exact(c, expm1(v * 0.69314718055994530942));
        fset(c, 0, v);
        break;
    }
    case 14: {
        static const double values[] = {0.0,
                                        1.0,
                                        3.14159265358979323846,
                                        0.69314718055994530942,
                                        1.44269504088896340736,
                                        0.30102999566398119521,
                                        3.32192809488736234787};
        fpush(c, values[a]);
        break;
    }
    case 15:
        wr16(addr, fstsw(c));
        break;
    default:
        why = "unsupported x87 operation";
        return false;
    }
    return true;
}
