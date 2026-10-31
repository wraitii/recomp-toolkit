#pragma once
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
    RP_SPACE_CONST = 0,
    RP_SPACE_RAM = 1,
    RP_SPACE_REGISTER = 2,
    RP_SPACE_UNIQUE = 3,
    RP_SPACE_OTHER = 4,
};

enum {
    RP_OP_COPY = 1,
    RP_OP_LOAD = 2,
    RP_OP_STORE = 3,
    RP_OP_BRANCH = 4,
    RP_OP_CBRANCH = 5,
    RP_OP_BRANCHIND = 6,
    RP_OP_CALL = 7,
    RP_OP_CALLIND = 8,
    RP_OP_CALLOTHER = 9,
    RP_OP_RETURN = 10,
    RP_OP_INT_EQUAL = 11,
    RP_OP_INT_NOTEQUAL = 12,
    RP_OP_INT_SLESS = 13,
    RP_OP_INT_SLESSEQUAL = 14,
    RP_OP_INT_LESS = 15,
    RP_OP_INT_LESSEQUAL = 16,
    RP_OP_INT_ZEXT = 17,
    RP_OP_INT_SEXT = 18,
    RP_OP_INT_ADD = 19,
    RP_OP_INT_SUB = 20,
    RP_OP_INT_CARRY = 21,
    RP_OP_INT_SCARRY = 22,
    RP_OP_INT_SBORROW = 23,
    RP_OP_INT_2COMP = 24,
    RP_OP_INT_NEGATE = 25,
    RP_OP_INT_XOR = 26,
    RP_OP_INT_AND = 27,
    RP_OP_INT_OR = 28,
    RP_OP_INT_LEFT = 29,
    RP_OP_INT_RIGHT = 30,
    RP_OP_INT_SRIGHT = 31,
    RP_OP_INT_MULT = 32,
    RP_OP_INT_DIV = 33,
    RP_OP_INT_SDIV = 34,
    RP_OP_INT_REM = 35,
    RP_OP_INT_SREM = 36,
    RP_OP_BOOL_NEGATE = 37,
    RP_OP_BOOL_XOR = 38,
    RP_OP_BOOL_AND = 39,
    RP_OP_BOOL_OR = 40,
    RP_OP_FLOAT_EQUAL = 41,
    RP_OP_FLOAT_NOTEQUAL = 42,
    RP_OP_FLOAT_LESS = 43,
    RP_OP_FLOAT_LESSEQUAL = 44,
    RP_OP_FLOAT_NAN = 45,
    RP_OP_FLOAT_ADD = 46,
    RP_OP_FLOAT_DIV = 47,
    RP_OP_FLOAT_MULT = 48,
    RP_OP_FLOAT_SUB = 49,
    RP_OP_FLOAT_NEG = 50,
    RP_OP_FLOAT_ABS = 51,
    RP_OP_FLOAT_SQRT = 52,
    RP_OP_FLOAT_INT2FLOAT = 53,
    RP_OP_FLOAT_FLOAT2FLOAT = 54,
    RP_OP_FLOAT_TRUNC = 55,
    RP_OP_FLOAT_CEIL = 56,
    RP_OP_FLOAT_FLOOR = 57,
    RP_OP_FLOAT_ROUND = 58,
    RP_OP_PIECE = 59,
    RP_OP_SUBPIECE = 60,
    RP_OP_POPCOUNT = 61,
    RP_OP_LZCOUNT = 62,
    RP_OP_FLOAT_MIN = 63,
    RP_OP_FLOAT_MAX = 64,
    RP_OP_CVTSS2SI = 65,
    RP_OP_CVTTSS2SI = 66,
    RP_OP_CVTSI2SS = 67,
    RP_OP_FLOAT_RCP = 68,
    RP_OP_FLOAT_RSQRT = 69,
    RP_OP_DIVMOD = 70,
    RP_OP_SDIVMOD = 71,
};

enum {
    RP_STRING_MOVS = 0,
    RP_STRING_STOS = 1,
    RP_STRING_LODS = 2,
    RP_STRING_CMPS = 3,
    RP_STRING_SCAS = 4,
    RP_STRING_FAMILIES = 5,
};

enum {
    RP_STRING_ONCE = 0,
    RP_STRING_REPE = 1,
    RP_STRING_REPNE = 2,
    RP_STRING_MODES = 3,
};

enum {
    RP_STRING_WIDTHS = 3,
    RP_USEROP_STRING_BASE = 0x1000,
    RP_USEROP_STRING_END =
        RP_USEROP_STRING_BASE + RP_STRING_FAMILIES * RP_STRING_WIDTHS * RP_STRING_MODES,
};

#define RP_USEROP_RDTSC UINT64_MAX
#define RP_USEROP_CPUID (UINT64_MAX - 1)
#define RP_USEROP_X87_CLEAR (UINT64_MAX - 2)
#define RP_USEROP_X87_EMPTY (UINT64_MAX - 3)
#define RP_USEROP_X87_EMPTY_RESET (UINT64_MAX - 4)

typedef struct RpVarnode {
    uint32_t space;
    uint32_t size;
    uint64_t offset;
} RpVarnode;

typedef struct RpOp {
    uint32_t opcode;
    RpVarnode output;
    uint32_t has_output;
    uint64_t userop;
    uint32_t has_userop;
    uint32_t input_count;
    RpVarnode inputs[4];
} RpOp;

typedef struct RpLift {
    uint32_t length;
    uint32_t op_count;
    RpOp *ops;
    char *error;
} RpLift;

int rp_lift(uint32_t addr, const uint8_t *data, size_t len, RpLift *out);
void rp_free(RpOp *ops, uint32_t count);
const char *rp_error(void);
const char *rp_mnemonic(void);
typedef int (*RpJitCompile)(const char *input, const char *directory);
void rp_jit_note(uint32_t target, int is_call, uint32_t base, const char *sha256,
                 RpJitCompile compile);
char *rp_jit_take(void);
void rp_jit_free_path(char *path);
void rp_jit_module_status(const char *directory, const char *status, const char *reason,
                          const uint32_t *addresses, const uint64_t *calls,
                          const uint64_t *handoffs, int resume_safe, uint32_t count);
void rp_jit_shutdown(void);

#ifdef __cplusplus
}
#endif
