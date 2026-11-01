#include "pcode.h"
#include "discovery.h"
#include "jit.h"
#include "pcode_core.h"
#include "pcode_numeric.h"
#include "pcode_x87.h"
#include "../platform/os.h"

#include <algorithm>
#include <climits>
#include <cstddef>
#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <exception>
#include <string>
#include <unordered_map>
#include <vector>

extern "C" int recomp_thunk_target_kind(uint32_t);

namespace {
const uint32_t kMaxDepth = 256;
const uint32_t kMaxInstruction = 15;
const uint32_t kMaxCache = 65536;

struct Cached {
    uint8_t bytes[kMaxInstruction];
    uint8_t len;
    std::shared_ptr<PcodeInstruction> instruction;
};

using BaselineRead = uint64_t (*)(void *, uint64_t, uint32_t);
using BaselineWrite = void (*)(void *, uint64_t, uint32_t, uint64_t);
struct BaselineLocation {
    uint32_t pc, length, index;
};
static_assert(offsetof(BaselineLocation, length) == 4 && offsetof(BaselineLocation, index) == 8);
using BaselineReadMemory = int (*)(void *, uint64_t, uint32_t, uint64_t *);
using BaselineWriteMemory = int (*)(void *, uint64_t, uint32_t, uint64_t);
using BaselineUserop = int (*)(void *, uint64_t, uint64_t, uint32_t, uint64_t *);
using BaselineScalar = int (*)(void *, uint64_t, uint64_t, uint64_t, uint64_t *);
using BaselineCode = int (*)(void *, BaselineRead, BaselineWrite, BaselineLocation *,
                             BaselineReadMemory, BaselineWriteMemory, X86 *, uint64_t *,
                             BaselineUserop, BaselineScalar);
struct ExecutionFrame;
struct ThreadState {
    unsigned depth = 0;
    char error[512]{};
    std::unordered_map<uint32_t, Cached> cache;
    std::deque<uint32_t> cache_order;
    std::unordered_map<uint32_t, const RpBaselineBlock *> baseline;
    std::vector<std::unique_ptr<RpBaselineBlock>> baseline_blocks;
    std::vector<std::unique_ptr<ExecutionFrame>> frames;
    uint32_t countdown = 0;
    std::chrono::steady_clock::time_point next = std::chrono::steady_clock::now();
    ~ThreadState();
};
thread_local ThreadState g_state;
std::atomic<uint64_t> g_baseline_compiles{0}, g_baseline_hits{0};
std::atomic<uint64_t> g_baseline_executions{0};
std::atomic<uint64_t> g_baseline_instructions{0};
std::atomic<uint64_t> g_baseline_compile_us{0};

void baseline_report() {
    static const auto start = std::chrono::steady_clock::now();
    const auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
                             std::chrono::steady_clock::now() - start)
                             .count();
    fprintf(stdout,
            "[recomp] baseline: elapsed_ms=%lld compiled=%llu executions=%llu instructions=%llu "
            "cache_hits=%llu compile_us=%llu\n",
            (long long)elapsed, (unsigned long long)g_baseline_compiles.load(),
            (unsigned long long)g_baseline_executions.load(),
            (unsigned long long)g_baseline_instructions.load(),
            (unsigned long long)g_baseline_hits.load(),
            (unsigned long long)g_baseline_compile_us.load());
    fflush(stdout);
}

void baseline_progress(ThreadState &state) {
    if (state.countdown++ % 65536 != 0)
        return;
    const auto now = std::chrono::steady_clock::now();
    if (now < state.next)
        return;
    state.next = now + std::chrono::seconds(2);
    baseline_report();
}

bool in_arena(uint32_t addr, uint32_t size) {
    return addr >= GUEST_NULL_LIMIT && size != 0 && addr < GUEST_SIZE && size <= GUEST_SIZE - addr;
}

uint64_t read_words(const uint32_t *words, uint64_t offset, uint32_t size) {
    uint32_t byte = offset % 4;
    if (size == 4 && byte == 0)
        return words[offset / 4];
    if (size >= 1 && size <= 4 - byte)
        return (words[offset / 4] >> (byte * 8)) & (UINT32_MAX >> ((4 - size) * 8));
    if (size == 8 && byte == 0)
        return words[offset / 4] | (uint64_t)words[offset / 4 + 1] << 32;
    uint64_t value = 0;
    for (uint32_t i = 0; i < size; ++i) {
        uint64_t byte = offset + i;
        value |= (uint64_t)((words[byte / 4] >> ((byte % 4) * 8)) & 0xff) << (8 * i);
    }
    return value;
}

void write_words(uint32_t *words, uint64_t offset, uint32_t size, uint64_t value) {
    uint32_t byte = offset % 4;
    if (size == 4 && byte == 0) {
        words[offset / 4] = (uint32_t)value;
        return;
    }
    if (size >= 1 && size <= 4 - byte) {
        uint32_t shift = byte * 8;
        uint32_t mask = UINT32_MAX >> ((4 - size) * 8);
        uint32_t &word = words[offset / 4];
        word = (word & ~(mask << shift)) | (((uint32_t)value & mask) << shift);
        return;
    }
    if (size == 8 && byte == 0) {
        words[offset / 4] = (uint32_t)value;
        words[offset / 4 + 1] = (uint32_t)(value >> 32);
        return;
    }
    for (uint32_t i = 0; i < size; ++i) {
        uint64_t byte = offset + i;
        uint32_t shift = (byte % 4) * 8;
        uint32_t &word = words[byte / 4];
        word = (word & ~(0xffu << shift)) | ((uint32_t)(value >> (8 * i)) & 0xffu) << shift;
    }
}

int flag_bit(uint64_t offset) {
    if (offset < 0x200 || offset > 0x214 || offset == 0x20c)
        return -1;
    return (int)(offset - 0x200) + (offset > 0x20c);
}

uint32_t *split_flag(X86 *c, uint64_t offset) {
    switch (offset) {
    case 0x200:
        return &c->eflags_cf;
    case 0x202:
        return &c->eflags_pf;
    case 0x204:
        return &c->eflags_af;
    case 0x206:
        return &c->eflags_zf;
    case 0x207:
        return &c->eflags_sf;
    case 0x20a:
        return &c->eflags_df;
    case 0x20b:
        return &c->eflags_of;
    default:
        return nullptr;
    }
}

enum class RegisterKind : uint8_t {
    Invalid,
    Gpr,
    Fs,
    Eflags,
    EflagsMisc,
    Eip,
    Flag,
    X87Control,
    Mm,
    Xmm
};

RegisterKind classify_register(const RpVarnode &v, const char *&what) {
    if (v.space != RP_SPACE_REGISTER) {
        what = "not a register";
        return RegisterKind::Invalid;
    }
    if (v.size == 0 || v.size > 8) {
        what = "register width is not modelled";
        return RegisterKind::Invalid;
    }
    if (v.offset >= 0xffff0000 && v.offset < 0xffff0004 && v.size <= 0xffff0004 - v.offset)
        return RegisterKind::EflagsMisc;
    if (v.offset == 0x300 && v.size == 2)
        return RegisterKind::X87Control;
    if (v.offset >= 0x1100 && v.offset < 0x1180 && v.offset % 16 < 8 && v.size <= 8 - v.offset % 16)
        return RegisterKind::Mm;
    if (v.offset >= 0x1200 && v.offset < 0x1400 && v.offset % 64 < 16 &&
        v.size <= 16 - v.offset % 64)
        return RegisterKind::Xmm;
    if (v.offset < 0x20 && v.offset + v.size <= 0x20 && v.size <= 4)
        return RegisterKind::Gpr;
    if (v.offset == 0x110 && v.size == 4)
        return RegisterKind::Fs;
    if (v.offset == 0x280 && (v.size == 2 || v.size == 4))
        return RegisterKind::Eflags;
    if (v.offset == 0x284 && (v.size == 2 || v.size == 4))
        return RegisterKind::Eip;
    if (v.offset >= 0x200 && v.offset + v.size <= 0x215 && v.size == 1)
        return RegisterKind::Flag;
    what = "unsupported guest register";
    return RegisterKind::Invalid;
}

const auto g_register_kinds = [] {
    std::array<std::array<RegisterKind, 8>, 0x1400> kinds{};
    for (uint32_t offset = 0; offset < kinds.size(); ++offset) {
        for (uint32_t size = 1; size <= 8; ++size) {
            const char *what = nullptr;
            kinds[offset][size - 1] = classify_register({RP_SPACE_REGISTER, size, offset}, what);
        }
    }
    return kinds;
}();

RegisterKind register_kind(const RpVarnode &v, const char *&what) {
    if (v.space == RP_SPACE_REGISTER && v.size >= 1 && v.size <= 8 &&
        v.offset < g_register_kinds.size()) {
        RegisterKind kind = g_register_kinds[v.offset][v.size - 1];
        if (kind != RegisterKind::Invalid)
            return kind;
    }
    return classify_register(v, what);
}

class X86Host final : public PcodeHost {
  public:
    explicit X86Host(X86 *c, ThreadState &state) : c_(c), state_(&state) {
        if (c->dynamic_return_count)
            continuation_ = c->dynamic_returns[c->dynamic_return_count - 1];
        if (in_arena(c->r[R_ESP], 4)) {
            const uint32_t target = rd32(c->r[R_ESP]);
            if (target == GUEST_RETURN_SENTINEL || recomp_is_call_return(target) ||
                recomp_module_is_call_return(target))
                continuation_ = target;
        }
    }

    ThreadState &state() const {
        return *state_;
    }

    X86 *cpu() const {
        return c_;
    }

    std::shared_ptr<PcodeInstruction> lift(uint64_t pc64, std::string &why) override {
        uint32_t pc;
        if (!guest_addr(pc64, pc, why))
            return nullptr;
        if (pc < GUEST_NULL_LIMIT) {
            why = "instruction fetch below null-page limit";
            return nullptr;
        }
        if (!in_arena(pc, 1)) {
            why = "instruction fetch outside guest memory";
            return nullptr;
        }
        uint32_t available = GUEST_SIZE - pc;
        if (available > kMaxInstruction)
            available = kMaxInstruction;
        auto it = state_->cache.find(pc);
        if (it != state_->cache.end() && it->second.len <= available &&
            memcmp(it->second.bytes, g_mem + pc, it->second.len) == 0) {
            return it->second.instruction;
        }
        RpLift raw{};
        if (!rp_lift(pc, g_mem + pc, available, &raw)) {
            const char *message = rp_error();
            why = message ? message : "SLEIGH lift failed";
            return nullptr;
        }
        if (raw.length == 0 || raw.length > available) {
            rp_free(raw.ops, raw.op_count);
            why = "SLEIGH length outside supplied bytes";
            return nullptr;
        }
        auto instruction = std::make_shared<PcodeInstruction>();
        instruction->length = raw.length;
        auto &ops = instruction->ops;
        ops.assign(raw.ops, raw.ops + raw.op_count);
        rp_free(raw.ops, raw.op_count);
        if (it == state_->cache.end()) {
            if (state_->cache.size() >= kMaxCache) {
                state_->cache.erase(state_->cache_order.front());
                state_->cache_order.pop_front();
            }
            state_->cache_order.push_back(pc);
        }
        Cached cached{};
        memcpy(cached.bytes, g_mem + pc, instruction->length);
        cached.len = (uint8_t)instruction->length;
        cached.instruction = instruction;
        state_->cache[pc] = std::move(cached);
        return instruction;
    }

    bool register_supported(const RpVarnode &v, std::string &why) override {
        const char *what = nullptr;
        RegisterKind kind = register_kind(v, what);
        if (kind == RegisterKind::Invalid && v.space == RP_SPACE_REGISTER && v.offset >= 0x1000 &&
            v.offset < 0xffff0000) {
            why = "x87/MMX/SSE register p-code is not modelled";
            return false;
        }
        if (kind == RegisterKind::Invalid ||
            (kind == RegisterKind::Flag && flag_bit(v.offset) < 0)) {
            if (kind == RegisterKind::Flag)
                what = "flag";
            char message[128];
            snprintf(message, sizeof message, "%s at register offset 0x%llx size %u", what,
                     (unsigned long long)v.offset, v.size);
            why = message;
            return false;
        }
        return true;
    }

    bool read_register(const RpVarnode &v, uint64_t &out, std::string &why) override {
        const char *what = nullptr;
        RegisterKind kind = register_kind(v, what);
        if (kind == RegisterKind::Invalid) {
            why = what;
            return false;
        }
        if (kind == RegisterKind::X87Control) {
            out = c_->fpu_cw;
            return true;
        }
        if (kind == RegisterKind::Gpr) {
            out = read_words(c_->r, v.offset, v.size);
            return true;
        }
        if (kind == RegisterKind::Mm) {
            out = c_->mm[(v.offset - 0x1100) / 16] >> ((v.offset % 16) * 8);
            out &= UINT64_MAX >> ((8 - v.size) * 8);
            return true;
        }
        if (kind == RegisterKind::Xmm) {
            out = read_words(c_->xmm[(v.offset - 0x1200) / 64], v.offset % 64, v.size);
            return true;
        }
        if (kind == RegisterKind::Fs) {
            out = c_->fs_base;
            return true;
        }
        if (kind == RegisterKind::Eip) {
            out = v.size == 2 ? (c_->eip & 0xffffu) : c_->eip;
            return true;
        }
        if (kind == RegisterKind::EflagsMisc) {
            out = (c_->eflags_misc >> ((v.offset - 0xffff0000) * 8)) &
                  (UINT32_MAX >> ((4 - v.size) * 8));
            return true;
        }
        if (kind == RegisterKind::Flag) {
            if (uint32_t *field = split_flag(c_, v.offset)) {
                x86_cc_settle(c_);
                out = *field & 1u;
                return true;
            }
        }
        uint32_t flags = x86_get_eflags(c_);
        if (kind == RegisterKind::Eflags) {
            out = v.size == 2 ? (flags & 0xffffu) : flags;
            return true;
        }
        int bit = flag_bit(v.offset);
        if (bit < 0) {
            why = "unsupported individual flag";
            return false;
        }
        out = (flags >> bit) & 1;
        return true;
    }

    bool write_register(const RpVarnode &v, uint64_t value, std::string &why) override {
        const char *what = nullptr;
        RegisterKind kind = register_kind(v, what);
        if (kind == RegisterKind::Invalid) {
            why = what;
            return false;
        }
        if (kind == RegisterKind::X87Control) {
            x87_set_cw(c_, (uint16_t)value);
            return true;
        }
        if (kind == RegisterKind::Gpr) {
            write_words(c_->r, v.offset, v.size, value);
            return true;
        }
        if (kind == RegisterKind::Mm) {
            uint64_t &word = c_->mm[(v.offset - 0x1100) / 16];
            uint32_t shift = (uint32_t)(v.offset % 16) * 8;
            uint64_t mask = UINT64_MAX >> ((8 - v.size) * 8);
            word = (word & ~(mask << shift)) | ((value & mask) << shift);
            return true;
        }
        if (kind == RegisterKind::Xmm) {
            write_words(c_->xmm[(v.offset - 0x1200) / 64], v.offset % 64, v.size, value);
            return true;
        }
        if (kind == RegisterKind::Fs) {
            c_->fs_base = (uint32_t)value;
            return true;
        }
        if (kind == RegisterKind::Eip) {
            c_->eip = v.size == 2 ? (c_->eip & 0xffff0000u) | ((uint32_t)value & 0xffffu)
                                  : (uint32_t)value;
            return true;
        }
        if (kind == RegisterKind::EflagsMisc) {
            uint32_t shift = (uint32_t)(v.offset - 0xffff0000) * 8;
            uint32_t mask = UINT32_MAX >> ((4 - v.size) * 8);
            c_->eflags_misc =
                (c_->eflags_misc & ~(mask << shift)) | (((uint32_t)value & mask) << shift);
            return true;
        }
        if (kind == RegisterKind::Flag) {
            if (uint32_t *field = split_flag(c_, v.offset)) {
                x86_cc_settle(c_);
                *field = (uint32_t)value & 1u;
                c_->eflags_misc = (c_->eflags_misc & ~X86_EFLAGS_SPLIT) | 2u;
                return true;
            }
        }
        uint32_t flags = x86_get_eflags(c_);
        if (kind == RegisterKind::Eflags) {
            if (v.size == 2)
                flags = (flags & 0xffff0000u) | ((uint32_t)value & 0xffffu);
            else
                flags = (uint32_t)value;
            x86_set_eflags(c_, flags);
            return true;
        }
        int bit = flag_bit(v.offset);
        if (bit < 0) {
            why = "unsupported individual flag";
            return false;
        }
        flags = (flags & ~(1u << bit)) | (((uint32_t)value & 1u) << bit);
        x86_set_eflags(c_, flags);
        return true;
    }

    bool memory_supported(uint64_t addr64, uint32_t size, bool, std::string &why) override {
        uint32_t addr;
        if (!guest_addr(addr64, addr, why))
            return false;
        if (addr < GUEST_NULL_LIMIT) {
            why = "guest memory access below null-page limit";
            return false;
        }
        if (!in_arena(addr, size)) {
            why = "guest memory access outside arena (page permissions are not modelled)";
            return false;
        }
        return true;
    }

    bool read_memory(uint64_t addr, uint32_t size, uint64_t &out, std::string &why) override {
        if (!memory_supported(addr, size, false, why))
            return false;
        switch (size) {
        case 1:
            out = rd8((uint32_t)addr);
            break;
        case 2:
            out = rd16((uint32_t)addr);
            break;
        case 4:
            out = rd32((uint32_t)addr);
            break;
        case 8:
            out = rd64((uint32_t)addr);
            break;
        default:
            out = 0;
            for (uint32_t i = 0; i < size; ++i)
                out |= (uint64_t)rd8((uint32_t)addr + i) << (8 * i);
            break;
        }
        return true;
    }

    bool write_memory(uint64_t addr, uint32_t size, uint64_t value, std::string &why) override {
        if (!memory_supported(addr, size, true, why))
            return false;
        switch (size) {
        case 1:
            wr8((uint32_t)addr, (uint8_t)value);
            break;
        case 2:
            wr16((uint32_t)addr, (uint16_t)value);
            break;
        case 4:
            wr32((uint32_t)addr, (uint32_t)value);
            break;
        case 8:
            wr64((uint32_t)addr, value);
            break;
        default:
            for (uint32_t i = 0; i < size; ++i)
                wr8((uint32_t)addr + i, (uint8_t)(value >> (8 * i)));
            break;
        }
        return true;
    }

    bool float_supported(const RpOp &op, std::string &why) override {
        uint32_t count = 2;
        uint32_t size = 4;
        switch (op.opcode) {
        case RP_OP_FLOAT_NAN:
            count = 1;
            size = 1;
            break;
        case RP_OP_FLOAT_EQUAL:
        case RP_OP_FLOAT_NOTEQUAL:
        case RP_OP_FLOAT_LESS:
        case RP_OP_FLOAT_LESSEQUAL:
            size = 1;
            break;
        case RP_OP_FLOAT_NEG:
        case RP_OP_FLOAT_ABS:
        case RP_OP_FLOAT_SQRT:
        case RP_OP_FLOAT_RCP:
        case RP_OP_FLOAT_RSQRT:
            count = 1;
            break;
        case RP_OP_FLOAT_ADD:
        case RP_OP_FLOAT_SUB:
        case RP_OP_FLOAT_MULT:
        case RP_OP_FLOAT_DIV:
        case RP_OP_FLOAT_MIN:
        case RP_OP_FLOAT_MAX:
            break;
        default:
            why = "floating-point p-code operation is not modelled";
            return false;
        }
        if (!op.has_output || op.output.size != size || op.input_count != count) {
            why = "malformed floating-point p-code operation";
            return false;
        }
        for (uint32_t i = 0; i < count; ++i) {
            if (op.inputs[i].size != 4) {
                why = "floating-point p-code input width is not modelled";
                return false;
            }
        }
        return true;
    }

    bool execute_float(const RpOp &op, uint64_t left, uint64_t right, uint64_t &out,
                       std::string &why) override {
        uint32_t a = (uint32_t)left, b = (uint32_t)right;
        switch (op.opcode) {
        case RP_OP_FLOAT_NAN:
            out = recomp_sse_nan_bits(a);
            break;
        case RP_OP_FLOAT_EQUAL:
            out = recomp_sse_f32(a) == recomp_sse_f32(b);
            break;
        case RP_OP_FLOAT_NOTEQUAL:
            out = recomp_sse_f32(a) != recomp_sse_f32(b);
            break;
        case RP_OP_FLOAT_LESS:
            out = recomp_sse_f32(a) < recomp_sse_f32(b);
            break;
        case RP_OP_FLOAT_LESSEQUAL:
            out = recomp_sse_f32(a) <= recomp_sse_f32(b);
            break;
        case RP_OP_FLOAT_NEG:
            out = a ^ 0x80000000u;
            break;
        case RP_OP_FLOAT_ABS:
            out = a & 0x7fffffffu;
            break;
        case RP_OP_FLOAT_ADD:
            out = recomp_sse_addps(c_, a, b);
            break;
        case RP_OP_FLOAT_SUB:
            out = recomp_sse_subps(c_, a, b);
            break;
        case RP_OP_FLOAT_MULT:
            out = recomp_sse_mulps(c_, a, b);
            break;
        case RP_OP_FLOAT_DIV:
            out = recomp_sse_divps(c_, a, b);
            break;
        case RP_OP_FLOAT_MIN:
            out = recomp_sse_minps(a, b);
            break;
        case RP_OP_FLOAT_MAX:
            out = recomp_sse_maxps(a, b);
            break;
        case RP_OP_FLOAT_SQRT:
            out = recomp_sse_sqrtps(a);
            break;
        case RP_OP_FLOAT_RCP:
            out = recomp_sse_rcpps(a);
            break;
        case RP_OP_FLOAT_RSQRT:
            out = recomp_sse_rsqrtps(a);
            break;
        default:
            why = "floating-point p-code operation is not modelled";
            return false;
        }
        return true;
    }

    bool userop_supported(uint64_t id, const RpOp &op, std::string &why) override {
        if (pcode_x87_supported(id, op))
            return true;
        if ((id == RP_USEROP_RDTSC || id == RP_USEROP_CPUID || id == RP_USEROP_X87_CLEAR ||
             id == RP_USEROP_X87_EMPTY || id == RP_USEROP_X87_EMPTY_RESET ||
             (id >= RP_USEROP_STRING_BASE && id < RP_USEROP_STRING_END)) &&
            op.input_count == 1 && !op.has_output && op.inputs[0].offset == id)
            return true;
        char message[128];
        snprintf(message, sizeof message,
                 "unsupported userop id %llu (libsla exposes no userop-name API)",
                 (unsigned long long)id);
        why = message;
        return false;
    }

    bool execute_userop(const RpOp &op, const uint64_t *args, uint64_t &result,
                        std::string &why) override {
        if (!userop_supported(op.userop, op, why))
            return false;
        if (pcode_x87_supported(op.userop, op))
            return pcode_x87_execute(*this, c_, op.userop, op.input_count, args, result, why);
        if (op.userop == RP_USEROP_X87_EMPTY || op.userop == RP_USEROP_X87_EMPTY_RESET) {
            c_->fpu_tag = 0xffff;
            if (op.userop == RP_USEROP_X87_EMPTY_RESET)
                c_->fpu_top = 0;
            return true;
        }
        if (op.userop == RP_USEROP_X87_CLEAR) {
            c_->fpu_sw &= (uint16_t)~0x80ffu;
            return true;
        }
        if (op.userop == RP_USEROP_RDTSC) {
            recomp_rdtsc(c_);
            return true;
        }
        if (op.userop == RP_USEROP_CPUID) {
            recomp_cpuid(c_);
            return true;
        }
        using StringOp = void (*)(X86 *);
        static const StringOp functions[RP_STRING_FAMILIES][RP_STRING_WIDTHS] = {
            {movsb, movsw, movsd}, {stosb, stosw, stosd}, {lodsb, lodsw, lodsd},
            {cmpsb, cmpsw, cmpsd}, {scasb, scasw, scasd},
        };
        uint32_t code = (uint32_t)(op.userop - RP_USEROP_STRING_BASE);
        uint32_t mode = code % RP_STRING_MODES;
        uint32_t width = code / RP_STRING_MODES % RP_STRING_WIDTHS;
        uint32_t family = code / RP_STRING_MODES / RP_STRING_WIDTHS;
        uint32_t size = 1u << width;
        bool reads_source =
            family == RP_STRING_MOVS || family == RP_STRING_LODS || family == RP_STRING_CMPS;
        bool writes_destination = family == RP_STRING_MOVS || family == RP_STRING_STOS;
        bool compares = family == RP_STRING_CMPS || family == RP_STRING_SCAS;
        uint32_t iterations = 0;
        while (mode == RP_STRING_ONCE || c_->r[R_ECX]) {
            if (reads_source && !memory_supported(c_->r[R_ESI], size, false, why))
                return false;
            if (family != RP_STRING_LODS &&
                !memory_supported(c_->r[R_EDI], size, writes_destination, why))
                return false;
            functions[family][width](c_);
            if (mode == RP_STRING_ONCE)
                break;
            --c_->r[R_ECX];
            if ((++iterations & 1023u) == 0)
                recomp_execution_checkpoint(c_);
            if (compares && ((mode == RP_STRING_REPE && !c_->eflags_zf) ||
                             (mode == RP_STRING_REPNE && c_->eflags_zf)))
                break;
        }
        return true;
    }

    bool check_division_result(uint32_t opcode, uint32_t size, uint64_t quotient,
                               std::string &why) override {
        if (size != 2 && size != 4 && size != 8) {
            why = "x86 division operand width is not modelled";
            return false;
        }
        uint32_t bits = size * 4;
        bool fits;
        if (opcode == RP_OP_INT_SDIV) {
            uint32_t shift = 64 - size * 8;
            int64_t value = (int64_t)(quotient << shift) >> shift;
            int64_t limit = 1ll << (bits - 1);
            fits = value >= -limit && value < limit;
        } else {
            fits = (quotient >> bits) == 0;
        }
        if (!fits)
            why = "x86 division quotient overflow (guest exception delivery is not modelled)";
        return fits;
    }

    void checkpoint(uint64_t pc64) override {
        uint32_t pc;
        std::string why;
        if (guest_addr(pc64, pc, why))
            c_->eip = pc;
        recomp_execution_checkpoint(c_);
    }

    bool external_branch(uint64_t target64, uint64_t pc64, uint64_t &next_pc, bool &handled,
                         std::string &why) override {
        uint32_t target, pc;
        if (!guest_addr(target64, target, why) || !guest_addr(pc64, pc, why)) {
            why = "branch target outside guest memory";
            return false;
        }
        if (target < GUEST_NULL_LIMIT) {
            why = "branch target below null-page limit";
            return false;
        }
        if ((target >= GUEST_SHIM_BASE && target < GUEST_SHIM_END) ||
            recomp_thunk_target_kind(target) == 1) {
            c_->eip = pc;
            recomp_jump(c_, target);
            handled = true;
            return true;
        }
        discovery_note("jump", target, pc);
        next_pc = target;
        if (target <= pc && continuation_) {
            uint32_t next = target;
            const int result = recomp_jit_resume(c_, target, continuation_, &next);
            if (result) {
                x86_cc_settle(c_);
                next_pc = next;
                handled = result == 1;
                return true;
            }
        }
        handled = false;
        return true;
    }

    bool call(uint64_t target64, uint64_t pc64, uint64_t &next_pc, bool &handled,
              std::string &why) override {
        uint32_t target, pc;
        if (!guest_addr(target64, target, why) || !guest_addr(pc64, pc, why))
            return false;
        if (state_->depth >= kMaxDepth) {
            why = "interpreter host recursion limit";
            return false;
        }
        uint64_t expected;
        if (!read_memory(c_->r[R_ESP], 4, expected, why))
            return false;
        c_->eip = pc;
        const unsigned dynamic_depth = recomp_dynamic_return_push(c_, (uint32_t)expected);
        ++state_->depth;
        recomp_call(c_, target);
        --state_->depth;
        recomp_dynamic_return_pop(c_, dynamic_depth);
        x86_cc_settle(c_);
        next_pc = c_->eip;
        handled = next_pc != expected;
        if (!handled && recomp_thunk_target_kind(c_->eip) == 1) {
            recomp_jump(c_, c_->eip);
            handled = true;
        }
        return true;
    }

    bool return_to(uint64_t target64, uint64_t, std::string &why) override {
        uint32_t target;
        if (!guest_addr(target64, target, why))
            return false;
        c_->eip = target;
        return true;
    }

  private:
    static bool guest_addr(uint64_t value, uint32_t &out, std::string &why) {
        if (value > UINT32_MAX) {
            why = "guest address exceeds 32 bits";
            return false;
        }
        out = (uint32_t)value;
        return true;
    }

    X86 *c_;
    ThreadState *state_;
    uint32_t continuation_ = 0;
};

struct BaselineContext {
    BaselineContext(X86Host &host, const RpBaselineBlock &block, uint32_t start)
        : host(host), location{start, block.lengths[0], 0} {
        why[0] = 0;
    }

    X86Host &host;
    BaselineLocation location;
    char why[512];
};

uint64_t baseline_read(void *context, uint64_t offset, uint32_t size) {
    auto &host = static_cast<BaselineContext *>(context)->host;
    uint64_t value = 0;
    std::string why;
    if (!host.read_register({RP_SPACE_REGISTER, size, offset}, value, why)) {
        fprintf(stderr, "baseline register read: %s\n", why.c_str());
        abort();
    }
    return value;
}

void baseline_write(void *context, uint64_t offset, uint32_t size, uint64_t value) {
    auto &host = static_cast<BaselineContext *>(context)->host;
    std::string why;
    if (!host.write_register({RP_SPACE_REGISTER, size, offset}, value, why)) {
        fprintf(stderr, "baseline register write: %s\n", why.c_str());
        abort();
    }
}

int baseline_read_memory(void *opaque, uint64_t addr, uint32_t size, uint64_t *out) {
    auto &context = *static_cast<BaselineContext *>(opaque);
    std::string why;
    if (context.host.read_memory(addr, size, *out, why))
        return 0;
    snprintf(context.why, sizeof context.why, "%s", why.c_str());
    return 1;
}

int baseline_write_memory(void *opaque, uint64_t addr, uint32_t size, uint64_t value) {
    auto &context = *static_cast<BaselineContext *>(opaque);
    std::string why;
    if (context.host.write_memory(addr, size, value, why))
        return 0;
    snprintf(context.why, sizeof context.why, "%s", why.c_str());
    return 1;
}

int baseline_userop(void *opaque, uint64_t id, uint64_t argument, uint32_t input_count,
                    uint64_t *output) {
    auto &context = *static_cast<BaselineContext *>(opaque);
    try {
        RpOp op{};
        op.opcode = RP_OP_CALLOTHER;
        op.has_userop = 1;
        op.userop = id;
        op.input_count = input_count;
        op.inputs[0] = {RP_SPACE_CONST, 8, id};
        std::string why;
        if (context.host.execute_userop(op, &argument, *output, why))
            return 0;
        snprintf(context.why, sizeof context.why, "%s", why.c_str());
    } catch (const std::exception &error) {
        snprintf(context.why, sizeof context.why, "native userop exception: %s", error.what());
    } catch (const std::string &why) {
        snprintf(context.why, sizeof context.why, "%s", why.c_str());
    }
    return 1;
}

int baseline_scalar(void *opaque, uint64_t descriptor, uint64_t a, uint64_t b, uint64_t *out) {
    auto &context = *static_cast<BaselineContext *>(opaque);
    uint32_t opcode = descriptor & 255, size = (descriptor >> 8) & 255;
    std::string why;
    try {
        if (opcode == RP_OP_DIVMOD || opcode == RP_OP_SDIVMOD) {
            *out = pcode_divmod(opcode, (uint32_t)(descriptor >> 32), a, b);
            return 0;
        }
        if (opcode >= RP_OP_INT_DIV && opcode <= RP_OP_INT_SREM) {
            try {
                *out = pcode_divide(opcode, size, a, b);
            } catch (const std::string &reason) {
                throw std::string("division: ") + reason;
            }
            if ((opcode != RP_OP_INT_DIV && opcode != RP_OP_INT_SDIV) ||
                context.host.check_division_result(opcode, size, *out, why))
                return 0;
        } else {
            RpOp op{};
            op.opcode = opcode;
            op.has_output = 1;
            op.output.size = (descriptor >> 16) & 255;
            op.input_count = (descriptor >> 24) & 255;
            for (uint32_t i = 0; i < op.input_count; ++i)
                op.inputs[i].size = size;
            if (context.host.float_supported(op, why) &&
                context.host.execute_float(op, a, b, *out, why))
                return 0;
        }
    } catch (const std::string &reason) {
        why = reason;
    } catch (const std::exception &error) {
        why = error.what();
    }
    snprintf(context.why, sizeof context.why, "%s", why.c_str());
    return 1;
}

template <unsigned Kind, unsigned Bits, unsigned Flags>
int baseline_x87_special(void *opaque, uint64_t argument, uint32_t operand) {
    auto &context = *static_cast<BaselineContext *>(opaque);
    if constexpr (Bits != 0) {
        if (argument > UINT32_MAX) {
            snprintf(context.why, sizeof context.why, "invalid x87 memory operand");
            return 1;
        }
        if (argument < GUEST_NULL_LIMIT) {
            snprintf(context.why, sizeof context.why, "guest memory access below null-page limit");
            return 1;
        }
        if (!in_arena((uint32_t)argument, Bits / 8)) {
            snprintf(context.why, sizeof context.why,
                     "guest memory access outside arena (page permissions are not modelled)");
            return 1;
        }
    }
    try {
        X86 *c = context.host.cpu();
        unsigned a = operand & 255, extra = operand >> 8;
        if constexpr (Kind == 1)
            pcode_x87_push(c, Bits ? 8 : a, Bits, (uint32_t)argument);
        else if constexpr (Kind == 2)
            pcode_x87_push_integer(c, Bits, (uint32_t)argument);
        else if constexpr (Kind == 3)
            pcode_x87_store(c, Bits ? 8 : a, Bits, Flags & 1, Flags >> 1, (uint32_t)argument);
        else
            pcode_x87_arithmetic(c, Bits ? 0 : a, Bits, Flags, extra, (uint32_t)argument);
        return 0;
    } catch (const std::exception &error) {
        snprintf(context.why, sizeof context.why, "x87 helper exception: %s", error.what());
    } catch (...) {
        snprintf(context.why, sizeof context.why, "x87 helper exception");
    }
    return 1;
}

using BaselineX87Special = int (*)(void *, uint64_t, uint32_t);

template <unsigned Kind, unsigned Bits> BaselineX87Special baseline_x87_flags(unsigned flags) {
    if constexpr (Kind == 1 || Kind == 2)
        return baseline_x87_special<Kind, Bits, 0>;
    if constexpr (Kind == 3) {
        static constexpr BaselineX87Special choices[] = {
            baseline_x87_special<Kind, Bits, 0>, baseline_x87_special<Kind, Bits, 1>,
            baseline_x87_special<Kind, Bits, 2>, baseline_x87_special<Kind, Bits, 3>};
        return choices[flags];
    } else if constexpr (Kind == 4) {
        static constexpr BaselineX87Special choices[] = {
            baseline_x87_special<Kind, Bits, 0>,  baseline_x87_special<Kind, Bits, 1>,
            baseline_x87_special<Kind, Bits, 2>,  baseline_x87_special<Kind, Bits, 3>,
            baseline_x87_special<Kind, Bits, 4>,  baseline_x87_special<Kind, Bits, 5>,
            baseline_x87_special<Kind, Bits, 6>,  baseline_x87_special<Kind, Bits, 7>,
            baseline_x87_special<Kind, Bits, 8>,  baseline_x87_special<Kind, Bits, 9>,
            baseline_x87_special<Kind, Bits, 10>, baseline_x87_special<Kind, Bits, 11>,
            baseline_x87_special<Kind, Bits, 12>, baseline_x87_special<Kind, Bits, 13>,
            baseline_x87_special<Kind, Bits, 14>, baseline_x87_special<Kind, Bits, 15>};
        return choices[flags];
    }
    return nullptr;
}

template <unsigned Kind> BaselineX87Special baseline_x87_width(unsigned bits, unsigned flags) {
    switch (bits) {
    case 0:
        return baseline_x87_flags<Kind, 0>(flags);
    case 16:
        return baseline_x87_flags<Kind, 16>(flags);
    case 32:
        return baseline_x87_flags<Kind, 32>(flags);
    case 64:
        return baseline_x87_flags<Kind, 64>(flags);
    case 80:
        return baseline_x87_flags<Kind, 80>(flags);
    default:
        return nullptr;
    }
}

uintptr_t baseline_x87_select(uint64_t id) {
    unsigned kind = id & 255, bits = (id >> 16) & 255, flags = (id >> 24) & 15;
    unsigned extra = (id >> 28) & 15;
    BaselineX87Special result = nullptr;
    switch (kind) {
    case 1:
        result = baseline_x87_width<1>(bits, 0);
        break;
    case 2:
        result = baseline_x87_width<2>(bits, 0);
        break;
    case 3:
        result = baseline_x87_width<3>(bits, flags | (extra << 1));
        break;
    case 4:
        result = baseline_x87_width<4>(bits, flags);
        break;
    }
    return reinterpret_cast<uintptr_t>(result);
}

int baseline_step(X86Host &host, uint64_t &pc) {
    if (pc > UINT32_MAX || !in_arena((uint32_t)pc, 1))
        throw PcodeFault{"baseline instruction address outside arena", pc, pc, 0};
    auto &state = host.state();
    uint32_t start = (uint32_t)pc;
    uint32_t available = GUEST_SIZE - start;
    auto it = state.baseline.find(start);
    for (;;) {
        if (it == state.baseline.end()) {
            RpBaselineRejection rejection{};
            auto block = std::make_unique<RpBaselineBlock>();
            const auto time = std::chrono::steady_clock::now();
            const uint32_t flag_offsets[] = {offsetof(X86, eflags_cf), offsetof(X86, eflags_pf),
                                             offsetof(X86, eflags_af), offsetof(X86, eflags_zf),
                                             offsetof(X86, eflags_sf), offsetof(X86, eflags_df),
                                             offsetof(X86, eflags_of)};
            static const bool jitdump = [] {
                const char *value = getenv("RECOMP_BASELINE_JITDUMP");
                return value && strcmp(value, "1") == 0;
            }();
            int result = rp_baseline_compile(
                start, g_mem + start, available, block.get(), offsetof(X86, r), flag_offsets,
                offsetof(X86, eflags_misc), offsetof(X86, eip), recomp_thunk_target_kind,
                &rejection, os_monotonic_ns, jitdump ? (uint32_t)os_process_id() : 0,
                jitdump ? os_profile_thread_id() : 0, reinterpret_cast<uintptr_t>(g_mem),
                GUEST_SIZE, GUEST_NULL_LIMIT, !RECOMP_STORE_HOOKS, baseline_x87_select);
            g_baseline_compile_us += std::chrono::duration_cast<std::chrono::microseconds>(
                                         std::chrono::steady_clock::now() - time)
                                         .count();
            if (result < 0) {
                const char *error = rp_error();
                throw std::string("baseline compilation failed: ") +
                    (error ? error : "unknown error");
            }
            if (result != 1) {
                char why[320];
                snprintf(why, sizeof why, "baseline cannot compile %s: %s (reason %u, pcode %u)",
                         rejection.mnemonic[0] ? rejection.mnemonic : "instruction",
                         rejection.reason == 6 ? "native block allocation limit reached"
                         : rejection.detail[0] ? rejection.detail
                                               : "unsupported operation or operand shape",
                         rejection.reason, rejection.opcode);
                throw PcodeFault{why, start, start, rejection.length};
            }
            ++g_baseline_compiles;
            const auto *compiled = block.get();
            state.baseline_blocks.push_back(std::move(block));
            if (state.baseline.size() >= kMaxCache)
                state.baseline.clear();
            it = state.baseline.insert_or_assign(start, compiled).first;
        } else {
            ++g_baseline_hits;
        }
        if (memcmp(it->second->bytes, g_mem + start, it->second->byte_count) == 0)
            break;
        state.baseline.erase(it);
        it = state.baseline.end();
    }
    const RpBaselineBlock &block = *it->second;
    ++g_baseline_executions;
    BaselineContext context{host, block, start};
    x86_cc_settle(host.cpu());
    auto code = reinterpret_cast<BaselineCode>(const_cast<void *>(block.code));
    uint64_t result[3]{};
    int status =
        code(&context, baseline_read, baseline_write, &context.location, baseline_read_memory,
             baseline_write_memory, host.cpu(), result, baseline_userop, baseline_scalar);
    g_baseline_instructions += context.location.index + 1;
    if (status == 3) {
        bool handled = false;
        std::string why;
        if (!host.external_branch(result[0], context.location.pc, pc, handled, why))
            throw PcodeFault{why, context.location.pc, context.location.pc,
                             context.location.length};
        return handled ? -1 : 1;
    }
    if (status == 8) {
        bool handled = false;
        std::string why;
        if (!host.call(result[0], context.location.pc, pc, handled, why))
            throw PcodeFault{why, context.location.pc, context.location.pc,
                             context.location.length};
        return handled ? -1 : 1;
    }
    if (status == 9) {
        std::string why;
        if (!host.return_to(result[0], context.location.pc, why))
            throw PcodeFault{why, context.location.pc, context.location.pc,
                             context.location.length};
        return -1;
    }
    if (status == 10 || status == 11)
        throw PcodeFault{status == 10 ? "undefined unique read"
                                      : "p-code operation budget exhausted",
                         context.location.pc, context.location.pc, context.location.length};
    if (status == 7)
        throw PcodeFault{"guest address exceeds 32 bits", context.location.pc, context.location.pc,
                         context.location.length};
    if (status == 5 || status == 6)
        throw PcodeFault{
            status == 5 ? "guest memory access below null-page limit"
                        : "guest memory access outside arena (page permissions are not modelled)",
            context.location.pc, context.location.pc, context.location.length};
    if (status == 2)
        throw PcodeFault{context.why, context.location.pc, context.location.pc,
                         context.location.length};
    if (status != 0)
        throw std::string("baseline returned invalid status");
    static std::atomic<bool> reported{false};
    if (!reported.load(std::memory_order_relaxed) && !reported.exchange(true)) {
        fprintf(stderr,
                "[recomp] baseline: first native block completed at %08x (%u instructions)\n",
                start, block.count);
        fflush(stderr);
    }
    pc = uint64_t(context.location.pc) + context.location.length;
    return true;
}

struct ExecutionFrame {
    explicit ExecutionFrame(X86 *c, ThreadState &state) : host(c, state) {}
    X86Host host;
    std::unique_ptr<PcodeMachine> interpreter;
};

ThreadState::~ThreadState() = default;

void report_fault(ThreadState &state, const PcodeFault &fault) {
    snprintf(state.error, sizeof state.error, "%s at %08llx (pc %08llx, bytes", fault.why.c_str(),
             (unsigned long long)fault.addr, (unsigned long long)fault.pc);
    size_t used = strlen(state.error);
    for (uint32_t i = 0; i < fault.len && i < 15 && fault.addr < GUEST_SIZE &&
                         i < GUEST_SIZE - fault.addr && used + 4 < sizeof state.error;
         ++i)
        used += snprintf(state.error + used, sizeof state.error - used, " %02x",
                         rd8((uint32_t)fault.addr + i));
    snprintf(state.error + used, sizeof state.error - used, ")");
}
template <bool Baseline> int execute_call(X86 *c, uint32_t target) {
    auto &state = g_state;
    state.error[0] = 0;
    ++state.depth;
    struct Leave {
        ThreadState &state;
        ~Leave() {
            --state.depth;
        }
    } leave{state};
    x86_cc_settle(c);
    if (state.frames.size() <= state.depth)
        state.frames.resize(state.depth + 1);
    if (!state.frames[state.depth])
        state.frames[state.depth] = std::make_unique<ExecutionFrame>(c, state);
    auto &frame = *state.frames[state.depth];
    frame.host = X86Host(c, state);
    auto &host = frame.host;
    if constexpr (!Baseline) {
        if (!frame.interpreter)
            frame.interpreter = std::make_unique<PcodeMachine>(host);
    }
    static const bool announced = [] {
        if constexpr (!Baseline) {
            fprintf(stderr, "[recomp] execution backend: p-code interpreter\n");
            return false;
        }
        const char *verify = getenv("RECOMP_BASELINE_VERIFY");
        fprintf(stderr,
                "[recomp] execution backend: Cranelift baseline "
                "(verifier %s)\n",
                verify && strcmp(verify, "1") == 0 ? "on" : "off");
        baseline_report();
        std::atexit(baseline_report);
        return true;
    }();
    (void)announced;
    uint64_t pc = target;
    try {
        for (;;) {
            if (pc <= UINT32_MAX && recomp_thunk_target_kind((uint32_t)pc) == 1) {
                c->eip = (uint32_t)pc;
                recomp_jump(c, (uint32_t)pc);
                return 1;
            }
            host.checkpoint(pc);
            if constexpr (Baseline) {
                baseline_progress(state);
                if (baseline_step(host, pc) < 0)
                    return 1;
            } else {
                if (!frame.interpreter->step(pc))
                    return 1;
            }
        }
    } catch (const PcodeFault &fault) {
        c->eip = fault.pc;
        report_fault(state, fault);
        return -1;
    } catch (const std::string &why) {
        PcodeFault fault{why, pc, pc, 0};
        c->eip = pc;
        report_fault(state, fault);
        return -1;
    }
}

} // namespace

extern "C" int pcode_interp_call(X86 *c, uint32_t target) {
    return execute_call<false>(c, target);
}

extern "C" int pcode_baseline_call(X86 *c, uint32_t target) {
    return execute_call<true>(c, target);
}

extern "C" const char *pcode_interp_last_error(void) {
    return g_state.error;
}
