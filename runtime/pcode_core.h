#pragma once
#include "pcode_abi.h"

#include <array>
#include <cstdint>
#include <memory>
#include <string>
#include <unordered_map>
#include <vector>

struct PcodeInstruction {
    uint32_t length = 0;
    std::vector<RpOp> ops;
    bool preflighted = false;
};

struct PcodeHost {
    virtual ~PcodeHost() = default;
    virtual std::shared_ptr<PcodeInstruction> lift(uint64_t pc, std::string &why) = 0;
    virtual bool register_supported(const RpVarnode &v, std::string &why) = 0;
    virtual bool read_register(const RpVarnode &v, uint64_t &out, std::string &why) = 0;
    virtual bool write_register(const RpVarnode &v, uint64_t value, std::string &why) = 0;
    virtual bool memory_supported(uint64_t addr, uint32_t size, bool write, std::string &why) = 0;
    virtual bool read_memory(uint64_t addr, uint32_t size, uint64_t &out, std::string &why) = 0;
    virtual bool write_memory(uint64_t addr, uint32_t size, uint64_t value, std::string &why) = 0;
    virtual bool userop_supported(uint64_t id, const RpOp &op, std::string &why) = 0;
    virtual bool float_supported(const RpOp &, std::string &why) {
        why = "floating-point p-code is not modelled";
        return false;
    }
    virtual bool execute_float(const RpOp &, uint64_t, uint64_t, uint64_t &, std::string &why) {
        why = "floating-point p-code is not modelled";
        return false;
    }
    virtual bool execute_userop(const RpOp &, const uint64_t *, uint64_t &, std::string &why) {
        why = "unsupported userop execution";
        return false;
    }
    virtual bool check_division_result(uint32_t, uint32_t, uint64_t, std::string &) {
        return true;
    }
    virtual void checkpoint(uint64_t pc) = 0;
    virtual bool external_branch(uint64_t target, uint64_t pc, uint64_t &next_pc, bool &handled,
                                 std::string &why) = 0;
    virtual bool call(uint64_t target, uint64_t pc, uint64_t &next_pc, bool &handled,
                      std::string &why) = 0;
    virtual bool return_to(uint64_t target, uint64_t pc, std::string &why) = 0;
};

struct PcodeFault {
    std::string why;
    uint64_t pc;
    uint64_t addr;
    uint32_t len;
};

class PcodeMachine {
  public:
    explicit PcodeMachine(PcodeHost &host) : host_(host) {}
    bool step(uint64_t &pc);

  private:
    PcodeHost &host_;
    struct UniqueChunk {
        uint64_t generation = 0;
        uint64_t value = 0;
        uint8_t defined = 0;
    };
    struct UniqueSlot {
        uint64_t offset = 0;
        UniqueChunk *chunk = nullptr;
    };
    std::unordered_map<uint64_t, UniqueChunk> unique_;
    std::array<UniqueSlot, 256> unique_slots_{};
    uint64_t unique_generation_ = 0;
    uint64_t pc_ = 0;
    uint32_t len_ = 0;

    [[noreturn]] void fail(const std::string &why) const;
    UniqueChunk *unique_chunk(uint64_t offset, bool write);
    bool load(const RpVarnode &v, uint64_t &out);
    void store(const RpVarnode &v, uint64_t value);
    void preflight(const std::vector<RpOp> &ops);
};
