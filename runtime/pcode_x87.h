#pragma once
#include "pcode_core.h"
#include "x86.h"

bool pcode_x87_supported(uint64_t id, const RpOp &op);
bool pcode_x87_execute(PcodeHost &host, X86 *c, const RpOp &op, const uint64_t *args,
                       uint64_t &result, std::string &why);
