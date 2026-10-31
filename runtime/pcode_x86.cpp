#include "pcode.h"
#include "discovery.h"
#include "jit.h"
#include "pcode_core.h"

#include <climits>
#include <cstdio>
#include <cstring>
#include <deque>
#include <string>
#include <unordered_map>
#include <vector>

extern "C" int recomp_thunk_target_kind(uint32_t);

namespace {
const uint32_t kMaxDepth = 64;
const uint32_t kMaxInstruction = 15;
const uint32_t kMaxCache = 65536;

thread_local unsigned g_depth;
thread_local char g_error[512];

struct Cached {
    uint8_t bytes[kMaxInstruction];
    uint8_t len;
    std::shared_ptr<PcodeInstruction> instruction;
};
thread_local std::unordered_map<uint32_t, Cached> g_cache;
thread_local std::deque<uint32_t> g_cache_order;

bool in_arena(uint32_t addr, uint32_t size) {
    return addr >= GUEST_NULL_LIMIT && size != 0 && addr < GUEST_SIZE && size <= GUEST_SIZE - addr;
}

uint64_t read_gpr(const X86 *c, uint64_t offset, uint32_t size) {
    uint64_t value = 0;
    for (uint32_t i = 0; i < size; ++i) {
        uint64_t byte = offset + i;
        value |= (uint64_t)((c->r[byte / 4] >> ((byte % 4) * 8)) & 0xff) << (8 * i);
    }
    return value;
}

void write_gpr(X86 *c, uint64_t offset, uint32_t size, uint64_t value) {
    for (uint32_t i = 0; i < size; ++i) {
        uint64_t byte = offset + i;
        uint32_t shift = (byte % 4) * 8;
        uint32_t &word = c->r[byte / 4];
        word = (word & ~(0xffu << shift)) | ((uint32_t)(value >> (8 * i)) & 0xffu) << shift;
    }
}

int flag_bit(uint64_t offset) {
    if (offset < 0x200 || offset > 0x214 || offset == 0x20c)
        return -1;
    return (int)(offset - 0x200) + (offset > 0x20c);
}

enum class RegisterKind { Invalid, Gpr, Fs, Eflags, Eip, Flag, X87Control };

RegisterKind register_kind(const RpVarnode &v, const char *&what) {
    if (v.space != RP_SPACE_REGISTER) {
        what = "not a register";
        return RegisterKind::Invalid;
    }
    if (v.size == 0 || v.size > 8) {
        what = "register width is not modelled";
        return RegisterKind::Invalid;
    }
    if (v.offset == 0x300 && v.size == 2)
        return RegisterKind::X87Control;
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

class X86Host final : public PcodeHost {
  public:
    explicit X86Host(X86 *c) : c_(c) {
        if (c->dynamic_return_count)
            continuation_ = c->dynamic_returns[c->dynamic_return_count - 1];
        if (in_arena(c->r[R_ESP], 4)) {
            const uint32_t target = rd32(c->r[R_ESP]);
            if (target == GUEST_RETURN_SENTINEL || recomp_is_call_return(target) ||
                recomp_module_is_call_return(target))
                continuation_ = target;
        }
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
        auto it = g_cache.find(pc);
        if (it != g_cache.end() && it->second.len <= available &&
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
        if (it == g_cache.end()) {
            if (g_cache.size() >= kMaxCache) {
                g_cache.erase(g_cache_order.front());
                g_cache_order.pop_front();
            }
            g_cache_order.push_back(pc);
        }
        Cached cached{};
        memcpy(cached.bytes, g_mem + pc, instruction->length);
        cached.len = (uint8_t)instruction->length;
        cached.instruction = instruction;
        g_cache[pc] = std::move(cached);
        return instruction;
    }

    bool register_supported(const RpVarnode &v, std::string &why) override {
        if (v.space == RP_SPACE_REGISTER && v.offset >= 0x1000) {
            why = "x87/MMX/SSE register p-code is not modelled";
            return false;
        }
        const char *what = nullptr;
        RegisterKind kind = register_kind(v, what);
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
            out = read_gpr(c_, v.offset, v.size);
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
            write_gpr(c_, v.offset, v.size, value);
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

    bool userop_supported(uint64_t id, const RpOp &op, std::string &why) override {
        if ((id == RP_USEROP_RDTSC || id == RP_USEROP_CPUID || id == RP_USEROP_X87_CLEAR ||
             id == RP_USEROP_PUSHFD || id == RP_USEROP_POPFD ||
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

    bool execute_userop(const RpOp &op, std::string &why) override {
        if (!userop_supported(op.userop, op, why))
            return false;
        if (op.userop == RP_USEROP_PUSHFD || op.userop == RP_USEROP_POPFD) {
            bool push = op.userop == RP_USEROP_PUSHFD;
            uint32_t addr = c_->r[R_ESP] - (push ? 4u : 0u);
            if (!memory_supported(addr, 4, push, why))
                return false;
            if (push)
                recomp_pushfd(c_);
            else
                recomp_popfd(c_);
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
        if (g_depth >= kMaxDepth) {
            why = "interpreter host recursion limit";
            return false;
        }
        uint64_t expected;
        if (!read_memory(c_->r[R_ESP], 4, expected, why))
            return false;
        c_->eip = pc;
        const unsigned dynamic_depth = recomp_dynamic_return_push(c_, (uint32_t)expected);
        ++g_depth;
        recomp_call(c_, target);
        --g_depth;
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
    uint32_t continuation_ = 0;
};

void report_fault(const PcodeFault &fault) {
    snprintf(g_error, sizeof g_error, "%s at %08llx (pc %08llx, bytes", fault.why.c_str(),
             (unsigned long long)fault.addr, (unsigned long long)fault.pc);
    size_t used = strlen(g_error);
    for (uint32_t i = 0; i < fault.len && i < 15 && fault.addr < GUEST_SIZE &&
                         i < GUEST_SIZE - fault.addr && used + 4 < sizeof g_error;
         ++i)
        used +=
            snprintf(g_error + used, sizeof g_error - used, " %02x", rd8((uint32_t)fault.addr + i));
    snprintf(g_error + used, sizeof g_error - used, ")");
}
} // namespace

extern "C" int pcode_interp_call(X86 *c, uint32_t target) {
    g_error[0] = 0;
    ++g_depth;
    struct Leave {
        ~Leave() {
            --g_depth;
        }
    } leave;
    x86_cc_settle(c);
    X86Host host(c);
    PcodeMachine machine(host);
    uint64_t pc = target;
    try {
        for (;;) {
            if (pc <= UINT32_MAX && recomp_thunk_target_kind((uint32_t)pc) == 1) {
                c->eip = (uint32_t)pc;
                recomp_jump(c, (uint32_t)pc);
                return 1;
            }
            host.checkpoint(pc);
            if (!machine.step(pc))
                return 1;
        }
    } catch (const PcodeFault &fault) {
        c->eip = fault.pc;
        report_fault(fault);
        return -1;
    } catch (const std::string &why) {
        PcodeFault fault{why, pc, pc, 0};
        c->eip = pc;
        report_fault(fault);
        return -1;
    }
}

extern "C" const char *pcode_interp_last_error(void) {
    return g_error;
}
