#include "pcode_core.h"

#include <algorithm>
#include <cstdint>
#include <cstdio>

namespace {
const uint32_t kMaxOpsPerInstruction = 1u << 16;

uint64_t width_mask(uint32_t size) {
    if (size >= 8)
        return ~0ull;
    if (size == 0)
        return 0;
    return (1ull << (size * 8)) - 1ull;
}

bool is_float(uint32_t op) {
    switch (op) {
    case RP_OP_FLOAT_EQUAL:
    case RP_OP_FLOAT_NOTEQUAL:
    case RP_OP_FLOAT_LESS:
    case RP_OP_FLOAT_LESSEQUAL:
    case RP_OP_FLOAT_NAN:
    case RP_OP_FLOAT_ADD:
    case RP_OP_FLOAT_DIV:
    case RP_OP_FLOAT_MULT:
    case RP_OP_FLOAT_SUB:
    case RP_OP_FLOAT_NEG:
    case RP_OP_FLOAT_ABS:
    case RP_OP_FLOAT_SQRT:
    case RP_OP_FLOAT_INT2FLOAT:
    case RP_OP_FLOAT_FLOAT2FLOAT:
    case RP_OP_FLOAT_TRUNC:
    case RP_OP_FLOAT_CEIL:
    case RP_OP_FLOAT_FLOOR:
    case RP_OP_FLOAT_ROUND:
    case RP_OP_FLOAT_MIN:
    case RP_OP_FLOAT_MAX:
    case RP_OP_CVTSS2SI:
    case RP_OP_CVTTSS2SI:
    case RP_OP_CVTSI2SS:
    case RP_OP_FLOAT_RCP:
    case RP_OP_FLOAT_RSQRT:
        return true;
    default:
        return false;
    }
}

bool is_compare(uint32_t op) {
    return op >= RP_OP_INT_EQUAL && op <= RP_OP_INT_LESSEQUAL;
}

bool is_carry(uint32_t op) {
    return op == RP_OP_INT_CARRY || op == RP_OP_INT_SCARRY || op == RP_OP_INT_SBORROW;
}

bool is_bool(uint32_t op) {
    return op >= RP_OP_BOOL_NEGATE && op <= RP_OP_BOOL_OR;
}

bool is_unary(uint32_t op) {
    return op == RP_OP_INT_ZEXT || op == RP_OP_INT_SEXT || op == RP_OP_INT_2COMP ||
           op == RP_OP_INT_NEGATE || op == RP_OP_BOOL_NEGATE || op == RP_OP_POPCOUNT ||
           op == RP_OP_LZCOUNT;
}

bool is_binary(uint32_t op) {
    return op == RP_OP_INT_ADD || op == RP_OP_INT_SUB || op == RP_OP_INT_XOR ||
           op == RP_OP_INT_AND || op == RP_OP_INT_OR || op == RP_OP_INT_LEFT ||
           op == RP_OP_INT_RIGHT || op == RP_OP_INT_SRIGHT || op == RP_OP_INT_MULT ||
           op == RP_OP_INT_DIV || op == RP_OP_INT_SDIV || op == RP_OP_INT_REM ||
           op == RP_OP_INT_SREM;
}

uint64_t sign_extend(uint64_t value, uint32_t size) {
    uint32_t bits = size * 8;
    if (bits >= 64)
        return value;
    return (uint64_t)((int64_t)(value << (64 - bits)) >> (64 - bits));
}

uint64_t signed_divide(uint64_t a, uint64_t b, uint32_t size, bool remainder) {
    if (b == 0)
        throw std::string(remainder ? "signed remainder by zero" : "signed divide by zero");
    int64_t sa = (int64_t)sign_extend(a, size);
    int64_t sb = (int64_t)sign_extend(b, size);
    if (sa == INT64_MIN && sb == -1)
        return remainder ? 0 : (uint64_t)INT64_MIN;
    return (uint64_t)(remainder ? sa % sb : sa / sb);
}

uint64_t binary(uint32_t op, uint32_t size, uint64_t a, uint64_t b) {
    uint32_t bits = size * 8;
    uint64_t mask = width_mask(size);
    a &= mask;
    uint64_t count = b;
    b &= mask;
    switch (op) {
    case RP_OP_INT_EQUAL:
        return a == b;
    case RP_OP_INT_NOTEQUAL:
        return a != b;
    case RP_OP_INT_LESS:
        return a < b;
    case RP_OP_INT_LESSEQUAL:
        return a <= b;
    case RP_OP_INT_SLESS:
        return (int64_t)sign_extend(a, size) < (int64_t)sign_extend(b, size);
    case RP_OP_INT_SLESSEQUAL:
        return (int64_t)sign_extend(a, size) <= (int64_t)sign_extend(b, size);
    case RP_OP_INT_ADD:
        return (a + b) & mask;
    case RP_OP_INT_SUB:
        return (a - b) & mask;
    case RP_OP_INT_XOR:
        return (a ^ b) & mask;
    case RP_OP_INT_AND:
        return (a & b) & mask;
    case RP_OP_INT_OR:
        return (a | b) & mask;
    case RP_OP_INT_MULT:
        return (a * b) & mask;
    case RP_OP_INT_LEFT:
        return count >= bits ? 0 : (a << count) & mask;
    case RP_OP_INT_RIGHT:
        return count >= bits ? 0 : (a & mask) >> count;
    case RP_OP_INT_SRIGHT:
        return count >= bits ? (sign_extend(a, size) >> 63 ? mask : 0)
                             : (uint64_t)((int64_t)sign_extend(a, size) >> count) & mask;
    case RP_OP_INT_DIV:
        if (b == 0)
            throw std::string("integer divide by zero");
        return (a / b) & mask;
    case RP_OP_INT_REM:
        if (b == 0)
            throw std::string("integer remainder by zero");
        return (a % b) & mask;
    case RP_OP_INT_SDIV:
        return signed_divide(a, b, size, false) & mask;
    case RP_OP_INT_SREM:
        return signed_divide(a, b, size, true) & mask;
    case RP_OP_INT_CARRY:
        return a > mask - b;
    case RP_OP_INT_SCARRY: {
        uint64_t result = (a + b) & mask;
        bool sa = (a >> (bits - 1)) & 1, sb = (b >> (bits - 1)) & 1;
        bool sr = (result >> (bits - 1)) & 1;
        return sa == sb && sr != sa;
    }
    case RP_OP_INT_SBORROW: {
        uint64_t result = (a - b) & mask;
        bool sa = (a >> (bits - 1)) & 1, sb = (b >> (bits - 1)) & 1;
        bool sr = (result >> (bits - 1)) & 1;
        return sa != sb && sr != sa;
    }
    case RP_OP_BOOL_XOR:
        return (a ^ b) & 1;
    case RP_OP_BOOL_AND:
        return (a & b) & 1;
    case RP_OP_BOOL_OR:
        return (a | b) & 1;
    default:
        throw std::string("unsupported binary p-code operation");
    }
}
} // namespace

void PcodeMachine::fail(const std::string &why) const {
    throw PcodeFault{why, pc_, pc_, len_};
}

void PcodeInstruction::prepare_unique() {
    std::vector<uint64_t> offsets;
    auto collect = [&](const RpVarnode &v) {
        if (v.space == RP_SPACE_UNIQUE) {
            offsets.push_back(v.offset >> 3);
            offsets.push_back((v.offset + v.size - 1) >> 3);
        }
    };
    for (const auto &op : ops) {
        for (uint32_t i = 0; i < op.input_count; ++i)
            collect(op.inputs[i]);
        if (op.has_output)
            collect(op.output);
    }
    std::sort(offsets.begin(), offsets.end());
    offsets.erase(std::unique(offsets.begin(), offsets.end()), offsets.end());
    size_t rotation = 0;
    if (!offsets.empty() && offsets.front() == 0 && offsets.back() == (UINT64_MAX >> 3)) {
        auto gap = std::adjacent_find(offsets.begin(), offsets.end(),
                                      [](uint64_t a, uint64_t b) { return b != a + 1; });
        rotation = gap + 1 - offsets.begin();
    }
    auto remap = [&](RpVarnode &v) {
        if (v.space != RP_SPACE_UNIQUE)
            return;
        size_t index =
            std::lower_bound(offsets.begin(), offsets.end(), v.offset >> 3) - offsets.begin();
        index = (index + offsets.size() - rotation) % offsets.size();
        v.offset = index * 8 + (v.offset & 7);
    };
    for (auto &op : ops) {
        for (uint32_t i = 0; i < op.input_count; ++i)
            remap(op.inputs[i]);
        if (op.has_output)
            remap(op.output);
    }
    unique_chunks = offsets.size();
}

bool PcodeMachine::load(const RpVarnode &v, uint64_t &out) {
    switch (v.space) {
    case RP_SPACE_CONST:
        out = v.offset & width_mask(v.size);
        return true;
    case RP_SPACE_REGISTER: {
        std::string why;
        if (!host_.read_register(v, out, why))
            fail(why);
        out &= width_mask(v.size);
        return true;
    }
    case RP_SPACE_UNIQUE: {
        uint32_t byte = v.offset & 7;
        if (v.size <= 8 - byte) {
            const UniqueChunk &chunk = unique_[v.offset >> 3];
            uint32_t defined = ((1u << v.size) - 1) << byte;
            if (chunk.generation != unique_generation_ || (chunk.defined & defined) != defined)
                fail("undefined unique read");
            out = (chunk.value >> (8 * byte)) & width_mask(v.size);
            return true;
        }
        out = 0;
        for (uint32_t i = 0; i < v.size;) {
            uint64_t offset = v.offset + i;
            uint32_t byte = offset & 7;
            uint32_t count = v.size - i;
            if (count > 8 - byte)
                count = 8 - byte;
            const UniqueChunk &chunk = unique_[offset >> 3];
            if (chunk.generation != unique_generation_)
                fail("undefined unique read");
            uint32_t defined = ((1u << count) - 1) << byte;
            if ((chunk.defined & defined) != defined)
                fail("undefined unique read");
            out |= ((chunk.value >> (8 * byte)) & width_mask(count)) << (8 * i);
            i += count;
        }
        return true;
    }
    case RP_SPACE_RAM: {
        std::string why;
        if (!host_.read_memory(v.offset, v.size, out, why))
            fail(why);
        return true;
    }
    default:
        fail("unsupported p-code input address space");
    }
}

void PcodeMachine::store(const RpVarnode &v, uint64_t value) {
    if (v.space == RP_SPACE_REGISTER) {
        std::string why;
        if (!host_.write_register(v, value, why))
            fail(why);
        return;
    }
    if (v.space == RP_SPACE_UNIQUE) {
        uint32_t byte = v.offset & 7;
        if (v.size <= 8 - byte) {
            UniqueChunk &chunk = unique_[v.offset >> 3];
            if (chunk.generation != unique_generation_) {
                chunk.generation = unique_generation_;
                chunk.defined = 0;
            }
            uint64_t mask = width_mask(v.size) << (8 * byte);
            chunk.value = (chunk.value & ~mask) | ((value << (8 * byte)) & mask);
            chunk.defined |= ((1u << v.size) - 1) << byte;
            return;
        }
        for (uint32_t i = 0; i < v.size;) {
            uint64_t offset = v.offset + i;
            uint32_t byte = offset & 7;
            uint32_t count = v.size - i;
            if (count > 8 - byte)
                count = 8 - byte;
            UniqueChunk &chunk = unique_[offset >> 3];
            if (chunk.generation != unique_generation_) {
                chunk.generation = unique_generation_;
                chunk.defined = 0;
            }
            uint64_t mask = width_mask(count) << (8 * byte);
            chunk.value = (chunk.value & ~mask) | (((value >> (8 * i)) << (8 * byte)) & mask);
            chunk.defined |= ((1u << count) - 1) << byte;
            i += count;
        }
        return;
    }
    if (v.space == RP_SPACE_RAM) {
        std::string why;
        if (!host_.write_memory(v.offset, v.size, value, why))
            fail(why);
        return;
    }
    fail("unsupported p-code output address space");
}

void PcodeMachine::preflight(const std::vector<RpOp> &ops) {
    std::string why;
    auto width = [](const RpVarnode &v) { return v.size >= 1 && v.size <= 8; };
    auto value = [&](const RpVarnode &v, bool write = false) {
        if (!width(v) || (write && v.space == RP_SPACE_CONST))
            fail(write ? "unsupported p-code output" : "p-code value width is not modelled");
        if (v.space == RP_SPACE_CONST)
            return;
        if (v.space == RP_SPACE_REGISTER && !host_.register_supported(v, why))
            fail(why);
        if (v.space == RP_SPACE_RAM && !host_.memory_supported(v.offset, v.size, write, why))
            fail(why);
        if (v.space != RP_SPACE_REGISTER && v.space != RP_SPACE_UNIQUE && v.space != RP_SPACE_RAM)
            fail(write ? "unsupported p-code output address space"
                       : "unsupported p-code address space");
    };
    auto same = [&](const RpOp &op) {
        bool mismatch = false;
        for (uint32_t i = 1; i < op.input_count; ++i)
            mismatch |= op.inputs[i].size != op.inputs[0].size;
        if (mismatch) {
            char message[96];
            snprintf(message, sizeof message, "p-code operand widths differ for opcode %u (%u/%u)",
                     op.opcode, op.inputs[0].size, op.input_count > 1 ? op.inputs[1].size : 0);
            fail(message);
        }
    };

    for (const RpOp &op : ops) {
        if (op.opcode == 0)
            fail("unsupported p-code opcode");
        if (op.input_count > 4)
            fail("too many p-code inputs");
        if (op.opcode == RP_OP_CALLOTHER) {
            if (op.input_count < 1 || op.inputs[0].space != RP_SPACE_CONST || !op.has_userop)
                fail("malformed userop");
            if (!host_.userop_supported(op.userop, op, why))
                fail(why);
            for (uint32_t i = 1; i < op.input_count; ++i)
                value(op.inputs[i]);
            if (op.has_output)
                value(op.output, true);
            continue;
        }
        for (uint32_t i = 0; i < op.input_count; ++i) {
            bool selector = (op.opcode == RP_OP_LOAD || op.opcode == RP_OP_STORE) && i == 0;
            bool target = (op.opcode == RP_OP_BRANCH || op.opcode == RP_OP_CBRANCH ||
                           op.opcode == RP_OP_CALL) &&
                          i == 0;
            if (!selector && !target)
                value(op.inputs[i]);
        }
        if (op.has_output)
            value(op.output, true);
        if (is_float(op.opcode)) {
            if (!host_.float_supported(op, why))
                fail(why);
            continue;
        }

        switch (op.opcode) {
        case RP_OP_COPY:
            if (op.input_count != 1 || !op.has_output || op.output.size != op.inputs[0].size)
                fail("malformed COPY");
            break;
        case RP_OP_LOAD:
            if (op.input_count != 2 || !op.has_output || op.inputs[0].space != RP_SPACE_RAM ||
                !width(op.inputs[1]) || !width(op.output))
                fail("malformed LOAD");
            break;
        case RP_OP_STORE:
            if (op.input_count != 3 || op.has_output || op.inputs[0].space != RP_SPACE_RAM ||
                !width(op.inputs[1]) || !width(op.inputs[2]))
                fail("malformed STORE");
            break;
        case RP_OP_BRANCH:
        case RP_OP_CALL:
            if (op.input_count != 1 || op.has_output ||
                (op.inputs[0].space != RP_SPACE_CONST && op.inputs[0].space != RP_SPACE_RAM))
                fail("malformed direct control transfer");
            break;
        case RP_OP_CBRANCH:
            if (op.input_count != 2 || op.has_output || !width(op.inputs[1]) ||
                (op.inputs[0].space != RP_SPACE_CONST && op.inputs[0].space != RP_SPACE_RAM))
                fail("malformed conditional branch");
            break;
        case RP_OP_BRANCHIND:
        case RP_OP_CALLIND:
        case RP_OP_RETURN:
            if (op.input_count != 1 || op.has_output || !width(op.inputs[0]))
                fail("malformed indirect control transfer");
            break;
        case RP_OP_DIVMOD:
        case RP_OP_SDIVMOD:
            if (op.input_count != 3 || !op.has_output || op.output.size != 8 ||
                op.inputs[0].size != op.inputs[1].size || op.inputs[2].space != RP_SPACE_CONST ||
                (op.inputs[2].offset != 1 && op.inputs[2].offset != 2 &&
                 op.inputs[2].offset != 4) ||
                op.inputs[0].size != op.inputs[2].offset * 2)
                fail("malformed x86 division");
            break;
        case RP_OP_PIECE:
            if (op.input_count != 2 || !op.has_output ||
                op.inputs[0].size + op.inputs[1].size != op.output.size || op.output.size > 8)
                fail("malformed PIECE");
            break;
        case RP_OP_SUBPIECE:
            if (op.input_count != 2 || !op.has_output || op.output.size > op.inputs[0].size)
                fail("malformed SUBPIECE");
            break;
        case RP_OP_INT_ZEXT:
        case RP_OP_INT_SEXT:
        case RP_OP_POPCOUNT:
        case RP_OP_LZCOUNT:
            if (op.input_count != 1 || !op.has_output)
                fail("malformed unary p-code operation");
            break;
        default:
            if (is_compare(op.opcode) || is_carry(op.opcode)) {
                if (op.input_count != 2 || !op.has_output || op.output.size != 1)
                    fail("malformed comparison or carry");
                same(op);
            } else if (is_bool(op.opcode)) {
                if (op.opcode == RP_OP_BOOL_NEGATE) {
                    if (op.input_count != 1 || !op.has_output || op.output.size != 1)
                        fail("malformed BOOL_NEGATE");
                } else if (op.input_count != 2 || !op.has_output || op.output.size != 1) {
                    fail("malformed boolean p-code operation");
                }
            } else if (is_unary(op.opcode)) {
                if (op.input_count != 1 || !op.has_output ||
                    ((op.opcode == RP_OP_INT_2COMP || op.opcode == RP_OP_INT_NEGATE) &&
                     op.output.size != op.inputs[0].size))
                    fail("malformed unary p-code operation");
            } else if (is_binary(op.opcode)) {
                bool narrowed = op.opcode == RP_OP_INT_AND && op.output.size < op.inputs[0].size;
                if (op.input_count != 2 || !op.has_output ||
                    (op.output.size != op.inputs[0].size && !narrowed))
                    fail("malformed binary p-code operation");
                if (op.opcode != RP_OP_INT_LEFT && op.opcode != RP_OP_INT_RIGHT &&
                    op.opcode != RP_OP_INT_SRIGHT)
                    same(op);
            } else {
                fail("unsupported p-code opcode");
            }
            break;
        }
    }
}

bool PcodeMachine::step(uint64_t &pc) {
    pc_ = pc;
    std::string why;
    auto instruction = host_.lift(pc, why);
    if (!instruction)
        fail(why);
    len_ = instruction->length;
    if (len_ == 0)
        fail("p-code instruction length is not modelled");
    const auto &ops = instruction->ops;
    if (!instruction->preflighted) {
        preflight(ops);
        instruction->prepare_unique();
        instruction->preflighted = true;
    }
    if (unique_.size() < instruction->unique_chunks)
        unique_.resize(instruction->unique_chunks);
    if (++unique_generation_ == 0) {
        std::fill(unique_.begin(), unique_.end(), UniqueChunk{});
        unique_generation_ = 1;
    }
    uint64_t next = pc + len_;
    uint32_t ops_left = kMaxOpsPerInstruction;
    for (size_t ip = 0; ip < ops.size();) {
        if (ops_left-- == 0)
            fail("p-code operation budget exhausted");
        const RpOp &op = ops[ip];
        uint64_t a = 0, b = 0, out = 0;
        if (is_float(op.opcode)) {
            load(op.inputs[0], a);
            if (op.input_count > 1)
                load(op.inputs[1], b);
            if (!host_.execute_float(op, a, b, out, why))
                fail(why);
            store(op.output, out);
            ++ip;
            continue;
        }
        switch (op.opcode) {
        case RP_OP_COPY:
            load(op.inputs[0], a);
            store(op.output, a);
            break;
        case RP_OP_LOAD:
            load(op.inputs[1], a);
            if (!host_.read_memory(a, op.output.size, out, why))
                fail(why);
            store(op.output, out);
            break;
        case RP_OP_STORE:
            load(op.inputs[1], a);
            load(op.inputs[2], b);
            if (!host_.write_memory(a, op.inputs[2].size, b, why))
                fail(why);
            break;
        case RP_OP_BRANCH:
        case RP_OP_BRANCHIND:
        case RP_OP_CBRANCH: {
            bool conditional = op.opcode == RP_OP_CBRANCH;
            if (conditional) {
                load(op.inputs[1], b);
                if (b == 0) {
                    break;
                }
            }
            const RpVarnode &target = op.inputs[0];
            if (target.space == RP_SPACE_CONST) {
                int64_t rel = (int64_t)sign_extend(target.offset, target.size);
                int64_t at = (int64_t)ip + rel;
                if (at < 0 || (size_t)at > ops.size())
                    fail("internal p-code branch outside instruction");
                ip = (size_t)at;
                if (ip == ops.size())
                    next = pc + len_;
                continue;
            }
            uint64_t dest;
            if (op.opcode == RP_OP_BRANCHIND) {
                load(op.inputs[0], a);
                dest = a;
            } else {
                dest = target.offset;
            }
            bool handled = false;
            if (!host_.external_branch(dest, pc, next, handled, why))
                fail(why);
            if (handled)
                return false;
            ip = ops.size();
            continue;
        }
        case RP_OP_CALL:
        case RP_OP_CALLIND: {
            uint64_t target;
            if (op.opcode == RP_OP_CALL) {
                target = op.inputs[0].offset;
            } else {
                load(op.inputs[0], a);
                target = a;
            }
            bool handled = false;
            if (!host_.call(target, pc, next, handled, why))
                fail(why);
            if (handled)
                return false;
            ip = ops.size();
            continue;
        }
        case RP_OP_RETURN:
            load(op.inputs[0], a);
            if (!host_.return_to(a, pc, why))
                fail(why);
            return false;
        case RP_OP_CALLOTHER: {
            uint64_t args[3] = {}, result = 0;
            for (uint32_t i = 1; i < op.input_count; ++i)
                load(op.inputs[i], args[i - 1]);
            if (!host_.execute_userop(op, args, result, why))
                fail(why);
            if (op.has_output)
                store(op.output, result);
            break;
        }
        case RP_OP_INT_ZEXT:
            load(op.inputs[0], a);
            store(op.output, a & width_mask(op.inputs[0].size));
            break;
        case RP_OP_INT_SEXT:
            load(op.inputs[0], a);
            store(op.output, sign_extend(a, op.inputs[0].size) & width_mask(op.output.size));
            break;
        case RP_OP_INT_2COMP:
            load(op.inputs[0], a);
            store(op.output, (0ull - a) & width_mask(op.output.size));
            break;
        case RP_OP_INT_NEGATE:
            load(op.inputs[0], a);
            store(op.output, ~a & width_mask(op.output.size));
            break;
        case RP_OP_BOOL_NEGATE:
            load(op.inputs[0], a);
            store(op.output, (~a) & 1);
            break;
        case RP_OP_POPCOUNT:
            load(op.inputs[0], a);
            store(op.output, (uint64_t)__builtin_popcountll(a & width_mask(op.inputs[0].size)));
            break;
        case RP_OP_LZCOUNT: {
            load(op.inputs[0], a);
            uint32_t bits = op.inputs[0].size * 8;
            uint64_t v = a & width_mask(op.inputs[0].size);
            uint32_t n = 0;
            while (n < bits && ((v >> (bits - 1 - n)) & 1) == 0)
                ++n;
            store(op.output, n);
            break;
        }
        case RP_OP_DIVMOD:
        case RP_OP_SDIVMOD: {
            load(op.inputs[0], a);
            load(op.inputs[1], b);
            unsigned bits = (unsigned)op.inputs[2].offset * 8;
            uint64_t mask = (1ull << bits) - 1;
            uint64_t q, r;
            if (!b)
                fail("x86 division by zero");
            if (op.opcode == RP_OP_SDIVMOD) {
                int64_t n = (int64_t)sign_extend(a, op.inputs[0].size);
                int64_t d = (int64_t)sign_extend(b, op.inputs[1].size);
                if (n == INT64_MIN && d == -1)
                    fail("x86 division quotient overflow");
                int64_t sq = n / d;
                int64_t limit = 1ll << (bits - 1);
                if (sq < -limit || sq >= limit)
                    fail("x86 division quotient overflow");
                q = (uint64_t)sq & mask;
                r = (uint64_t)(n % d) & mask;
            } else {
                q = a / b;
                if (q > mask)
                    fail("x86 division quotient overflow");
                r = a % b;
            }
            store(op.output, q | (r << (bits == 8 ? 8 : 32)));
            break;
        }
        case RP_OP_PIECE:
            load(op.inputs[0], a);
            load(op.inputs[1], b);
            store(op.output, ((a << (op.inputs[1].size * 8)) | b) & width_mask(op.output.size));
            break;
        case RP_OP_SUBPIECE:
            load(op.inputs[0], a);
            load(op.inputs[1], b);
            if (b >= 8)
                out = 0;
            else
                out = (a >> (b * 8)) & width_mask(op.output.size);
            store(op.output, out);
            break;
        default:
            load(op.inputs[0], a);
            load(op.inputs[1], b);
            try {
                out = binary(op.opcode, op.inputs[0].size, a, b);
            } catch (const std::string &reason) {
                fail("division: " + reason);
            }
            if ((op.opcode == RP_OP_INT_DIV || op.opcode == RP_OP_INT_SDIV) &&
                !host_.check_division_result(op.opcode, op.inputs[0].size, out, why))
                fail(why);
            store(op.output, out);
            break;
        }
        ++ip;
    }
    pc = next;
    return true;
}
