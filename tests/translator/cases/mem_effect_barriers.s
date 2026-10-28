# mem_effect_barriers -- a REP string write and an x87 store must stop a dword
# load from being forwarded across them.
# input: eax=0 ecx=0
# input: eax=0xffffffff ecx=0x7fffffff

.section .data
barrier_buf:
    .long 0, 0, 0, 0
barrier_src:
    .byte 0x44, 0x33, 0x22, 0x11
barrier_float:
    .double 2.0

.set barrier_buf_va, DATA_BASE + (barrier_buf - data_start)
.set barrier_src_va, DATA_BASE + (barrier_src - data_start)
.set barrier_float_va, DATA_BASE + (barrier_float - data_start)

.text
.globl string_barrier
.type string_barrier, @function
string_barrier:
    mov dword ptr [barrier_buf_va], 0x11111111
    lea esi, [barrier_src_va]
    lea edi, [barrier_buf_va]
    mov ecx, 4
    rep movsb
    mov eax, [barrier_buf_va]
    ret
.size string_barrier, .-string_barrier

.globl x87_barrier
.type x87_barrier, @function
x87_barrier:
    mov dword ptr [barrier_buf_va + 8], 0x11111111
    fld qword ptr [barrier_float_va]
    fstp dword ptr [barrier_buf_va + 8]
    mov eax, [barrier_buf_va + 8]
    ret
.size x87_barrier, .-x87_barrier
