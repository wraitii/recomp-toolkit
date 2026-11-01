#include "baseline.h"
#include "baseline_x87.h"
#include "discovery.h"
#include "jit.h"
#include "../platform/os.h"

#include <array>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <iterator>
#include <memory>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

extern "C" int recomp_thunk_target_kind(uint32_t);

namespace {
namespace rb = rp::baseline;
using rb::FloatOp;
using rb::Status;
using rb::X87Kind;

constexpr uint32_t kMaxDepth = 256;
constexpr uint32_t kMaxCache = 65536;

struct ThreadState {
    unsigned depth = 0;
    char error[512]{};
    std::unordered_map<uint32_t, const rb::Block *> blocks;
    std::vector<std::unique_ptr<rb::Block>> storage;
    uint32_t countdown = 0;
    std::chrono::steady_clock::time_point next = std::chrono::steady_clock::now();
};
thread_local ThreadState g_state;

struct Statistics {
    std::atomic<uint64_t> compiles{0}, hits{0}, executions{0}, instructions{0}, compile_us{0};
} g_stats;

void report() {
    static const auto start = std::chrono::steady_clock::now();
    const auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
                             std::chrono::steady_clock::now() - start)
                             .count();
    fprintf(stdout,
            "[recomp] baseline: elapsed_ms=%lld compiled=%llu executions=%llu instructions=%llu "
            "cache_hits=%llu compile_us=%llu\n",
            (long long)elapsed, (unsigned long long)g_stats.compiles.load(),
            (unsigned long long)g_stats.executions.load(),
            (unsigned long long)g_stats.instructions.load(),
            (unsigned long long)g_stats.hits.load(), (unsigned long long)g_stats.compile_us.load());
    fflush(stdout);
}

void progress(ThreadState &state) {
    if (state.countdown++ % 65536 != 0)
        return;
    const auto now = std::chrono::steady_clock::now();
    if (now < state.next)
        return;
    state.next = now + std::chrono::seconds(2);
    report();
}

const char *status_message(Status status) {
    switch (status) {
    case Status::WideAddress:
        return "guest address exceeds 32 bits";
    case Status::NullAccess:
        return "guest memory access below null-page limit";
    case Status::OutsideArena:
        return "guest memory access outside arena (page permissions are not modelled)";
    case Status::UndefinedUnique:
        return "undefined unique read";
    case Status::BudgetExhausted:
        return "p-code operation budget exhausted";
    case Status::DivideByZero:
        return "x86 division by zero (guest exception delivery is not modelled)";
    case Status::DivideOverflow:
        return "x86 division quotient overflow (guest exception delivery is not modelled)";
    default:
        return nullptr;
    }
}

const char *memory_fault(uint32_t addr, uint32_t size) {
    if (addr < GUEST_NULL_LIMIT)
        return status_message(Status::NullAccess);
    if (size == 0 || addr >= GUEST_SIZE || size > GUEST_SIZE - addr)
        return status_message(Status::OutsideArena);
    return nullptr;
}

struct Fault {
    std::string why;
    uint32_t pc;
    uint32_t length;
};

struct Context {
    explicit Context(X86 *cpu) : cpu(cpu) {}

    int fail(const char *reason) {
        snprintf(why, sizeof why, "%s", reason);
        return 1;
    }

    rb::Frame frame{};
    X86 *cpu;
    char why[512]{};
};
static_assert(offsetof(Context, frame) == 0);

Context &context(rb::Frame *frame) {
    return *reinterpret_cast<Context *>(frame);
}

bool translated_entry(uint32_t target) {
    return recomp_thunk_target_kind(target) == 1;
}

void write_memory(uint32_t addr, uint32_t size, uint64_t value) {
    switch (size) {
    case 1:
        wr8(addr, (uint8_t)value);
        break;
    case 2:
        wr16(addr, (uint16_t)value);
        break;
    case 4:
        wr32(addr, (uint32_t)value);
        break;
    case 8:
        wr64(addr, value);
        break;
    default:
        for (uint32_t i = 0; i < size; ++i)
            wr8(addr + i, (uint8_t)(value >> (8 * i)));
        break;
    }
}

int string_op(rb::Frame *frame, rb::StringFamily family, uint8_t width_log2, rb::Repeat repeat) {
    using rb::Repeat;
    using rb::StringFamily;
    using Function = void (*)(X86 *);
    static constexpr Function functions[][3] = {
        {movsb, movsw, movsd}, {stosb, stosw, stosd}, {lodsb, lodsw, lodsd},
        {cmpsb, cmpsw, cmpsd}, {scasb, scasw, scasd},
    };
    auto &ctx = context(frame);
    if (size_t(family) >= std::size(functions) || width_log2 >= std::size(functions[0]))
        return ctx.fail("invalid string helper");
    X86 *c = ctx.cpu;
    const uint32_t size = 1u << width_log2;
    const bool reads_source = family == StringFamily::Movs || family == StringFamily::Lods ||
                              family == StringFamily::Cmps;
    const bool compares = family == StringFamily::Cmps || family == StringFamily::Scas;
    uint32_t iterations = 0;
    while (repeat == Repeat::Once || c->r[R_ECX]) {
        const char *fault = reads_source ? memory_fault(c->r[R_ESI], size) : nullptr;
        if (!fault && family != StringFamily::Lods)
            fault = memory_fault(c->r[R_EDI], size);
        if (fault)
            return ctx.fail(fault);
        functions[size_t(family)][width_log2](c);
        if (repeat == Repeat::Once)
            break;
        --c->r[R_ECX];
        if ((++iterations & 1023u) == 0)
            recomp_execution_checkpoint(c);
        if (compares && ((repeat == Repeat::WhileEqual && !c->eflags_zf) ||
                         (repeat == Repeat::WhileNotEqual && c->eflags_zf)))
            break;
    }
    return 0;
}

int x87(rb::Frame *frame, const rb::X87Call *call, uint64_t argument, uint64_t *result) {
    auto &ctx = context(frame);
    try {
        std::string why;
        if (baseline_x87_execute(ctx.cpu, *call, (uint32_t)argument, *result, why))
            return 0;
        return ctx.fail(why.c_str());
    } catch (const std::exception &error) {
        return ctx.fail((std::string("x87 helper exception: ") + error.what()).c_str());
    }
}

using X87Specialized = int (*)(rb::Frame *, uint64_t, uint32_t);

template <X87Kind Kind, unsigned Bits, unsigned Flags>
int x87_special(rb::Frame *frame, uint64_t argument, uint32_t operand) {
    auto &ctx = context(frame);
    try {
        X86 *c = ctx.cpu;
        const uint32_t addr = (uint32_t)argument;
        const unsigned a = operand & 255, extra = operand >> 8;
        const unsigned source = Bits ? kX87MemoryOperand : a;
        if constexpr (Kind == X87Kind::Push)
            baseline_x87_push(c, source, Bits, addr);
        else if constexpr (Kind == X87Kind::PushInteger)
            baseline_x87_push_integer(c, Bits, addr);
        else if constexpr (Kind == X87Kind::Store)
            baseline_x87_store(c, source, Bits, Flags & 1, Flags >> 1, addr);
        else
            baseline_x87_arithmetic(c, Bits ? 0 : a, Bits, Flags, extra, addr);
        return 0;
    } catch (const std::exception &error) {
        return ctx.fail((std::string("x87 helper exception: ") + error.what()).c_str());
    }
}

template <X87Kind Kind, unsigned Bits, unsigned... Flags>
X87Specialized x87_flag_variant(unsigned flags, std::integer_sequence<unsigned, Flags...>) {
    static constexpr X87Specialized variants[] = {x87_special<Kind, Bits, Flags>...};
    return flags < sizeof...(Flags) ? variants[flags] : nullptr;
}

template <X87Kind Kind, unsigned Bits> X87Specialized x87_flags(unsigned flags) {
    constexpr unsigned count = Kind == X87Kind::Store ? 4 : Kind == X87Kind::Arithmetic ? 16 : 1;
    return x87_flag_variant<Kind, Bits>(flags, std::make_integer_sequence<unsigned, count>{});
}

template <X87Kind Kind> X87Specialized x87_width(unsigned bits, unsigned flags) {
    switch (bits) {
    case 0:
        return x87_flags<Kind, 0>(flags);
    case 16:
        return x87_flags<Kind, 16>(flags);
    case 32:
        return x87_flags<Kind, 32>(flags);
    case 64:
        return x87_flags<Kind, 64>(flags);
    case 80:
        return x87_flags<Kind, 80>(flags);
    default:
        return nullptr;
    }
}

X87Specialized x87_select(const rb::X87Call *call) {
    switch (call->kind) {
    case X87Kind::Push:
        return x87_width<X87Kind::Push>(call->bits, 0);
    case X87Kind::PushInteger:
        return x87_width<X87Kind::PushInteger>(call->bits, 0);
    case X87Kind::Store:
        return x87_width<X87Kind::Store>(call->bits, call->flags | (call->extra << 1));
    case X87Kind::Arithmetic:
        return x87_width<X87Kind::Arithmetic>(call->bits, call->flags);
    default:
        return nullptr;
    }
}

template <FloatOp Op> uint64_t float_op(const X86 *c, uint64_t left, uint64_t right) {
    const uint32_t a = (uint32_t)left, b = (uint32_t)right;
    if constexpr (Op == FloatOp::Add)
        return recomp_sse_addps(c, a, b);
    else if constexpr (Op == FloatOp::Sub)
        return recomp_sse_subps(c, a, b);
    else if constexpr (Op == FloatOp::Mul)
        return recomp_sse_mulps(c, a, b);
    else if constexpr (Op == FloatOp::Div)
        return recomp_sse_divps(c, a, b);
    else if constexpr (Op == FloatOp::Min)
        return recomp_sse_minps(a, b);
    else if constexpr (Op == FloatOp::Max)
        return recomp_sse_maxps(a, b);
    else if constexpr (Op == FloatOp::Sqrt)
        return recomp_sse_sqrtps(a);
    else if constexpr (Op == FloatOp::Rcp)
        return recomp_sse_rcpps(a);
    else if constexpr (Op == FloatOp::Rsqrt)
        return recomp_sse_rsqrtps(a);
    else if constexpr (Op == FloatOp::Equal)
        return recomp_sse_f32(a) == recomp_sse_f32(b);
    else if constexpr (Op == FloatOp::NotEqual)
        return recomp_sse_f32(a) != recomp_sse_f32(b);
    else if constexpr (Op == FloatOp::Less)
        return recomp_sse_f32(a) < recomp_sse_f32(b);
    else if constexpr (Op == FloatOp::LessEqual)
        return recomp_sse_f32(a) <= recomp_sse_f32(b);
    else
        return (uint64_t)recomp_sse_nan_bits(a);
}

template <typename Function> size_t address(Function *function) {
    return reinterpret_cast<size_t>(function);
}

template <size_t... Op>
void float_ops(std::array<size_t, sizeof...(Op)> &table, std::index_sequence<Op...>) {
    ((table[Op] = address(float_op<FloatOp(Op)>)), ...);
}

rb::Config config() {
    static const bool jitdump = [] {
        const char *value = getenv("RECOMP_BASELINE_JITDUMP");
        return value && strcmp(value, "1") == 0;
    }();
    rb::Config config{};
    config.cpu = {offsetof(X86, r),
                  offsetof(X86, eip),
                  offsetof(X86, eflags_misc),
                  offsetof(X86, fs_base),
                  offsetof(X86, fpu_cw),
                  offsetof(X86, fpu_sw),
                  offsetof(X86, fpu_tag),
                  offsetof(X86, fpu_top),
                  offsetof(X86, mm),
                  offsetof(X86, xmm),
                  {offsetof(X86, eflags_cf), offsetof(X86, eflags_pf), offsetof(X86, eflags_af),
                   offsetof(X86, eflags_zf), offsetof(X86, eflags_sf), offsetof(X86, eflags_df),
                   offsetof(X86, eflags_of)}};
    config.guest_base = reinterpret_cast<size_t>(g_mem);
    config.guest_size = GUEST_SIZE;
    config.null_limit = GUEST_NULL_LIMIT;
    config.store_hooks = RECOMP_STORE_HOOKS;
    auto &callbacks = config.callbacks;
    callbacks.translated_entry = address(translated_entry);
    callbacks.x87_select = address(x87_select);
    callbacks.write_memory = address(write_memory);
    callbacks.string = address(string_op);
    callbacks.rdtsc = address(recomp_rdtsc);
    callbacks.cpuid = address(recomp_cpuid);
    callbacks.x87 = address(x87);
    float_ops(callbacks.float_ops,
              std::make_index_sequence<std::tuple_size_v<decltype(callbacks.float_ops)>>{});
    callbacks.clock_ns = address(os_monotonic_ns);
    config.pid = jitdump ? (uint32_t)os_process_id() : 0;
    config.tid = jitdump ? os_profile_thread_id() : 0;
    return config;
}

const char *text(const uint8_t *bytes) {
    return reinterpret_cast<const char *>(bytes);
}

const rb::Block &block(ThreadState &state, uint32_t start) {
    auto it = state.blocks.find(start);
    if (it != state.blocks.end()) {
        ++g_stats.hits;
        if (memcmp(it->second->bytes.data(), g_mem + start, it->second->byte_count) == 0)
            return *it->second;
        state.blocks.erase(it);
    }
    rb::Rejection rejection{};
    auto compiled = std::make_unique<rb::Block>();
    const auto time = std::chrono::steady_clock::now();
    const auto result =
        rb::compile(start, g_mem + start, GUEST_SIZE - start, config(), *compiled, rejection);
    g_stats.compile_us += std::chrono::duration_cast<std::chrono::microseconds>(
                              std::chrono::steady_clock::now() - time)
                              .count();
    if (result == rb::CompileResult::Error)
        throw Fault{std::string("baseline compilation failed: ") + text(rejection.detail.data()),
                    start, 0};
    if (result != rb::CompileResult::Compiled) {
        char why[320];
        snprintf(why, sizeof why, "baseline cannot compile %s: %s",
                 rejection.mnemonic[0] ? text(rejection.mnemonic.data()) : "instruction",
                 rejection.detail[0] ? text(rejection.detail.data())
                                     : "unsupported operation or operand shape");
        throw Fault{why, start, rejection.length};
    }
    ++g_stats.compiles;
    const auto *entry = compiled.get();
    state.storage.push_back(std::move(compiled));
    if (state.blocks.size() >= kMaxCache)
        state.blocks.clear();
    state.blocks.insert_or_assign(start, entry);
    return *entry;
}

class Routine {
  public:
    Routine(X86 *c, ThreadState &state) : c_(c), state_(state) {
        if (c->dynamic_return_count)
            continuation_ = c->dynamic_returns[c->dynamic_return_count - 1];
        if (!memory_fault(c->r[R_ESP], 4)) {
            const uint32_t target = rd32(c->r[R_ESP]);
            if (target == GUEST_RETURN_SENTINEL || recomp_is_call_return(target) ||
                recomp_module_is_call_return(target))
                continuation_ = target;
        }
    }

    bool step(uint32_t &pc) {
        if (memory_fault(pc, 1))
            throw Fault{"baseline instruction address outside arena", pc, 0};
        const uint32_t start = pc;
        const rb::Block &compiled = block(state_, start);
        ++g_stats.executions;
        Context context(c_);
        x86_cc_settle(c_);
        using Code = int (*)(rb::Frame *, X86 *);
        const auto status = Status(reinterpret_cast<Code>(compiled.code)(&context.frame, c_));
        g_stats.instructions += context.frame.index + 1;
        uint32_t at = start;
        for (uint32_t i = 0; i < context.frame.index; ++i)
            at += compiled.lengths[i];
        const uint32_t length = compiled.lengths[context.frame.index];
        switch (status) {
        case Status::Fallthrough: {
            static std::atomic<bool> reported{false};
            if (!reported.exchange(true)) {
                fprintf(stderr,
                        "[recomp] baseline: first native block completed at %08x (%u "
                        "instructions)\n",
                        start, compiled.count);
                fflush(stderr);
            }
            pc = at + length;
            return true;
        }
        case Status::Branch:
            return branch(guest(context.frame.target, at, length), at, pc);
        case Status::Call:
            return call(guest(context.frame.target, at, length), at, length, pc);
        case Status::Return:
            c_->eip = guest(context.frame.target, at, length);
            return false;
        case Status::HelperFailed:
            throw Fault{context.why, at, length};
        default:
            const char *message = status_message(status);
            throw Fault{message ? message : "baseline returned invalid status", at, length};
        }
    }

  private:
    static uint32_t guest(uint64_t target, uint32_t at, uint32_t length) {
        if (target > UINT32_MAX)
            throw Fault{status_message(Status::WideAddress), at, length};
        return (uint32_t)target;
    }

    bool branch(uint32_t target, uint32_t at, uint32_t &pc) {
        if (target < GUEST_NULL_LIMIT)
            throw Fault{"branch target below null-page limit", at, 0};
        if ((target >= GUEST_SHIM_BASE && target < GUEST_SHIM_END) || translated_entry(target)) {
            c_->eip = at;
            recomp_jump(c_, target);
            return false;
        }
        discovery_note("jump", target, at);
        pc = target;
        if (target <= at && continuation_) {
            uint32_t next = target;
            const int result = recomp_jit_resume(c_, target, continuation_, &next);
            if (result) {
                x86_cc_settle(c_);
                pc = next;
                return result != 1;
            }
        }
        return true;
    }

    bool call(uint32_t target, uint32_t at, uint32_t length, uint32_t &pc) {
        if (state_.depth >= kMaxDepth)
            throw Fault{"baseline host recursion limit", at, length};
        if (const char *fault = memory_fault(c_->r[R_ESP], 4))
            throw Fault{fault, at, length};
        const uint32_t expected = rd32(c_->r[R_ESP]);
        c_->eip = at;
        const unsigned dynamic_depth = recomp_dynamic_return_push(c_, expected);
        ++state_.depth;
        recomp_call(c_, target);
        --state_.depth;
        recomp_dynamic_return_pop(c_, dynamic_depth);
        x86_cc_settle(c_);
        pc = c_->eip;
        if (pc != expected)
            return false;
        if (translated_entry(pc)) {
            recomp_jump(c_, pc);
            return false;
        }
        return true;
    }

    X86 *c_;
    ThreadState &state_;
    uint32_t continuation_ = 0;
};

void report_fault(ThreadState &state, const Fault &fault) {
    snprintf(state.error, sizeof state.error, "%s at %08x (bytes", fault.why.c_str(), fault.pc);
    size_t used = strlen(state.error);
    for (uint32_t i = 0; i < fault.length && i < 15 && fault.pc < GUEST_SIZE &&
                         i < GUEST_SIZE - fault.pc && used + 4 < sizeof state.error;
         ++i)
        used += snprintf(state.error + used, sizeof state.error - used, " %02x", rd8(fault.pc + i));
    snprintf(state.error + used, sizeof state.error - used, ")");
}

void announce() {
    static const bool announced = [] {
        const char *verify = getenv("RECOMP_BASELINE_VERIFY");
        fprintf(stderr, "[recomp] execution backend: Cranelift baseline (verifier %s)\n",
                verify && strcmp(verify, "1") == 0 ? "on" : "off");
        report();
        std::atexit(report);
        return true;
    }();
    (void)announced;
}

} // namespace

extern "C" int baseline_call(X86 *c, uint32_t target) {
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
    announce();
    Routine routine(c, state);
    uint32_t pc = target;
    try {
        for (;;) {
            if (translated_entry(pc)) {
                c->eip = pc;
                recomp_jump(c, pc);
                return 1;
            }
            c->eip = pc;
            recomp_execution_checkpoint(c);
            progress(state);
            if (!routine.step(pc))
                return 1;
        }
    } catch (const Fault &fault) {
        c->eip = fault.pc;
        report_fault(state, fault);
        return -1;
    }
}

extern "C" const char *baseline_last_error(void) {
    return g_state.error;
}
