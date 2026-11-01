#include "baseline_x87.h"

#include <cmath>

using rp::baseline::X87Kind;

bool baseline_x87_execute(X86 *c, const rp::baseline::X87Call &call, uint32_t addr,
                          uint64_t &result, std::string &why) {
    const unsigned a = call.operand, bits = call.bits, flags = call.flags, extra = call.extra;
    switch (call.kind) {
    case X87Kind::Push:
        baseline_x87_push(c, a, bits, addr);
        break;
    case X87Kind::PushInteger:
        baseline_x87_push_integer(c, bits, addr);
        break;
    case X87Kind::Store:
        baseline_x87_store(c, a, bits, flags, extra, addr);
        break;
    case X87Kind::Arithmetic:
        baseline_x87_arithmetic(c, a, bits, flags, extra, addr);
        break;
    case X87Kind::Compare: {
        double rhs = (flags & 4) ? 0.0
                     : bits      ? ((flags & 2) ? (double)baseline_x87_integer(addr, bits)
                                                : baseline_x87_real(addr, bits))
                                 : ST(c, a);
        if (flags & 1)
            fucom(c, ST(c, 0), rhs);
        else
            fcom(c, ST(c, 0), rhs);
        for (unsigned i = 0; i < extra; ++i)
            fdrop(c);
        break;
    }
    case X87Kind::CompareFlags:
        result = fcomi_flags_sw(&c->fpu_sw, ST(c, 0), ST(c, a), flags);
        if (extra)
            fdrop(c);
        break;
    case X87Kind::Exchange:
        fxch(c, a);
        break;
    case X87Kind::Free:
        ftag_put(c, (c->fpu_top + a) & 7u, FTAG_EMPTY);
        break;
    case X87Kind::Status:
        result = fstsw(c);
        break;
    case X87Kind::Init:
        x87_finit(c);
        break;
    case X87Kind::Rotate:
        c->fpu_top = (c->fpu_top + (a ? 1u : 7u)) & 7u;
        break;
    case X87Kind::Nop:
        break;
    case X87Kind::Unary: {
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
    case X87Kind::Constant: {
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
    case X87Kind::StoreStatus:
        wr16(addr, fstsw(c));
        break;
    case X87Kind::Examine:
        fxam(c);
        break;
    case X87Kind::Remainder:
        fset(c, 0, fprem_common(c, ST(c, 0), ST(c, 1), a));
        break;
    case X87Kind::Yl2x:
        fset(c, 1, fx87_exact(c, ST(c, 1) * log2(ST(c, 0))));
        fdrop(c);
        break;
    case X87Kind::Scale:
        fset(c, 0, fx87_exact(c, fscale(ST(c, 0), ST(c, 1))));
        break;
    case X87Kind::Tangent:
        fset(c, 0, fx87_exact(c, tan(ST(c, 0))));
        fpush(c, 1.0);
        break;
    default:
        why = "unsupported x87 operation";
        return false;
    }
    return true;
}
