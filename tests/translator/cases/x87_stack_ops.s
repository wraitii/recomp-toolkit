# x87_stack_ops -- register-stack depth, fxch, non-popping fst st(i), pop/reversed
# arithmetic, constants, fchs/fabs/fsqrt and x87 values surviving a local cdecl call.

.section .data
v_a: .double 1.5
v_b: .double 2.0
v_c: .double 4.0
v_eight: .double 8.0
v_two: .double 2.0
v_neg: .double -2.25
v_sq: .double 2.25
f32_a: .float 1.5
f32_b: .float 2.0
f32_c: .float 4.0
outs: .zero 128

.set v_a_va, DATA_BASE + (v_a - data_start)
.set v_b_va, DATA_BASE + (v_b - data_start)
.set v_c_va, DATA_BASE + (v_c - data_start)
.set v_eight_va, DATA_BASE + (v_eight - data_start)
.set v_two_va, DATA_BASE + (v_two - data_start)
.set v_neg_va, DATA_BASE + (v_neg - data_start)
.set v_sq_va, DATA_BASE + (v_sq - data_start)
.set f32_a_va, DATA_BASE + (f32_a - data_start)
.set f32_b_va, DATA_BASE + (f32_b - data_start)
.set f32_c_va, DATA_BASE + (f32_c - data_start)
.set outs_va, DATA_BASE + (outs - data_start)

.text
.globl x87_stack_ops
.type x87_stack_ops, @function
x87_stack_ops:
    fld qword ptr [v_a_va]
    fld qword ptr [v_b_va]
    fld qword ptr [v_c_va]
    fxch st(2)
    fst st(2)
    faddp st(1), st
    fstp qword ptr [outs_va]
    fstp qword ptr [outs_va + 8]
    fld qword ptr [v_eight_va]
    fld qword ptr [v_two_va]
    fsubp st(1), st
    fstp qword ptr [outs_va + 16]
    fld qword ptr [v_eight_va]
    fld qword ptr [v_two_va]
    fsubrp st(1), st
    fstp qword ptr [outs_va + 24]
    fld qword ptr [v_eight_va]
    fld qword ptr [v_two_va]
    fdivp st(1), st
    fstp qword ptr [outs_va + 32]
    fld qword ptr [v_eight_va]
    fld qword ptr [v_two_va]
    fdivrp st(1), st
    fstp qword ptr [outs_va + 40]
    fld qword ptr [v_a_va]
    fld qword ptr [v_b_va]
    fmulp st(1), st
    fstp qword ptr [outs_va + 48]
    fld qword ptr [v_neg_va]
    fchs
    fstp qword ptr [outs_va + 56]
    fld qword ptr [v_neg_va]
    fabs
    fstp qword ptr [outs_va + 64]
    fld qword ptr [v_sq_va]
    fsqrt
    fstp qword ptr [outs_va + 72]
    fldz
    fstp qword ptr [outs_va + 80]
    fld1
    fstp qword ptr [outs_va + 88]
    fldpi
    fstp qword ptr [outs_va + 96]
    fldpi
    fadd st(0), st
    fstp qword ptr [outs_va + 104]
    xor eax, eax
    ret
.size x87_stack_ops, .-x87_stack_ops

.type x87_helper, @function
x87_helper:
    push ebp
    mov ebp, esp
    fld dword ptr [ebp + 8]
    fld dword ptr [ebp + 12]
    fmulp st(1), st
    fld dword ptr [ebp + 16]
    faddp st(1), st
    pop ebp
    ret
.size x87_helper, .-x87_helper

.globl x87_helper_call
.type x87_helper_call, @function
x87_helper_call:
    push ebp
    mov ebp, esp
    push dword ptr [f32_c_va]
    push dword ptr [f32_b_va]
    push dword ptr [f32_a_va]
    call x87_helper
    add esp, 12
    fstp qword ptr [outs_va + 112]
    fld qword ptr [v_a_va]
    push dword ptr [f32_c_va]
    push dword ptr [f32_b_va]
    push dword ptr [f32_a_va]
    call x87_helper
    add esp, 12
    faddp st(1), st
    fstp qword ptr [outs_va + 120]
    pop ebp
    xor eax, eax
    ret
.size x87_helper_call, .-x87_helper_call
