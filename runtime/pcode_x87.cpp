#include "pcode_x87.h"

#include <cmath>

bool pcode_x87_supported(uint64_t id, const RpOp &op) {
    return (id >> 32) == 1 && (id & 255) >= 1 && (id & 255) <= 20 && op.input_count >= 1 &&
           op.input_count <= 2 && op.inputs[0].offset == id;
}

bool pcode_x87_execute(PcodeHost &host, X86 *c, uint64_t id, uint32_t input_count,
                       const uint64_t *args, uint64_t &result, std::string &why) {
    unsigned kind = id & 255, a = (id >> 8) & 255;
    unsigned bits = (id >> 16) & 255, flags = (id >> 24) & 15;
    unsigned extra = (id >> 28) & 15;
    bool memory = (kind == 1 && a == 8) || kind == 2 || kind == 15 || (kind == 3 && a == 8) ||
                  ((kind == 4 || kind == 5) && bits);
    uint32_t addr = memory ? (uint32_t)args[0] : 0;
    if (memory && (input_count != 2 || args[0] > UINT32_MAX ||
                   !host.memory_supported(args[0], bits / 8, kind == 3 || kind == 15, why))) {
        if (why.empty())
            why = "invalid x87 memory operand";
        return false;
    }
    switch (kind) {
    case 1:
        pcode_x87_push(c, a, bits, addr);
        break;
    case 2:
        pcode_x87_push_integer(c, bits, addr);
        break;
    case 3:
        pcode_x87_store(c, a, bits, flags, extra, addr);
        break;
    case 4:
        pcode_x87_arithmetic(c, a, bits, flags, extra, addr);
        break;
    case 5: {
        double rhs = (flags & 4) ? 0.0
                     : bits      ? ((flags & 2) ? (double)pcode_x87_integer(addr, bits)
                                                : pcode_x87_real(addr, bits))
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
    case 16:
        fxam(c);
        break;
    case 17:
        fset(c, 0, fprem_common(c, ST(c, 0), ST(c, 1), a));
        break;
    case 18:
        fset(c, 1, fx87_exact(c, ST(c, 1) * log2(ST(c, 0))));
        fdrop(c);
        break;
    case 19:
        fset(c, 0, fx87_exact(c, fscale(ST(c, 0), ST(c, 1))));
        break;
    case 20:
        fset(c, 0, fx87_exact(c, tan(ST(c, 0))));
        fpush(c, 1.0);
        break;
    default:
        why = "unsupported x87 operation";
        return false;
    }
    return true;
}
