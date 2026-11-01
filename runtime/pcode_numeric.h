#pragma once
#include "pcode_abi.h"

#include <climits>
#include <string>

inline uint64_t pcode_sign_extend(uint64_t value, uint32_t size) {
    uint32_t bits = size * 8;
    return bits >= 64 ? value : (uint64_t)((int64_t)(value << (64 - bits)) >> (64 - bits));
}

inline uint64_t pcode_divide(uint32_t opcode, uint32_t size, uint64_t a, uint64_t b) {
    uint64_t mask = size == 8 ? UINT64_MAX : (1ull << (size * 8)) - 1;
    a &= mask;
    b &= mask;
    bool remainder = opcode == RP_OP_INT_REM || opcode == RP_OP_INT_SREM;
    bool signed_op = opcode == RP_OP_INT_SDIV || opcode == RP_OP_INT_SREM;
    if (!b)
        throw std::string(
            signed_op ? (remainder ? "signed remainder by zero" : "signed divide by zero")
                      : (remainder ? "integer remainder by zero" : "integer divide by zero"));
    if (!signed_op)
        return (remainder ? a % b : a / b) & mask;
    int64_t n = (int64_t)pcode_sign_extend(a, size);
    int64_t d = (int64_t)pcode_sign_extend(b, size);
    if (n == INT64_MIN && d == -1)
        return remainder ? 0 : (uint64_t)INT64_MIN;
    return (uint64_t)(remainder ? n % d : n / d) & mask;
}

inline uint64_t pcode_divmod(uint32_t opcode, uint32_t size, uint64_t a, uint64_t b) {
    unsigned bits = size * 8;
    uint64_t mask = (1ull << bits) - 1;
    if (!b)
        throw std::string("x86 division by zero");
    uint64_t q, r;
    if (opcode == RP_OP_SDIVMOD) {
        int64_t n = (int64_t)pcode_sign_extend(a, size * 2);
        int64_t d = (int64_t)pcode_sign_extend(b, size * 2);
        if (n == INT64_MIN && d == -1)
            throw std::string("x86 division quotient overflow");
        int64_t sq = n / d;
        int64_t limit = 1ll << (bits - 1);
        if (sq < -limit || sq >= limit)
            throw std::string("x86 division quotient overflow");
        q = (uint64_t)sq & mask;
        r = (uint64_t)(n % d) & mask;
    } else {
        q = a / b;
        if (q > mask)
            throw std::string("x86 division quotient overflow");
        r = a % b;
    }
    return q | (r << (bits == 8 ? 8 : 32));
}
