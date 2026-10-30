# input: arg0=1 arg1=3
# input: arg0=3 arg1=1
# input: arg0=2 arg1=2
# input: arg0=0xfffffffd arg1=4

.section .data
factor: .float -1.5
outputs: .zero 64
.set factor_va, DATA_BASE + (factor - data_start)
.set outputs_va, DATA_BASE + (outputs - data_start)

.text
.globl x87_full_depth
.type x87_full_depth, @function
x87_full_depth:
    sub esp, 4
    fld1
    fld1
    fld1
    fld1
    fld1
    fld1
    mov ecx, 3
depth_loop:
    fild dword ptr [esp + 8]
    fmul dword ptr [factor_va]
    fild dword ptr [esp + 12]
    fmul dword ptr [factor_va]
    fstp dword ptr [esp]
    fcom dword ptr [esp]
    fnstsw ax
    test ah, 5
    jp depth_x_less
    faddp st(6), st
    fadd dword ptr [esp]
    jmp depth_x_join
depth_x_less:
    fld dword ptr [esp]
    faddp st(7), st
    faddp st(1), st
depth_x_join:
    fild dword ptr [esp + 8]
    fmul dword ptr [factor_va]
    fild dword ptr [esp + 12]
    fmul dword ptr [factor_va]
    fstp dword ptr [esp]
    fcom dword ptr [esp]
    fnstsw ax
    test ah, 5
    jp depth_y_less
    faddp st(5), st
    fld dword ptr [esp]
    jmp depth_y_join
depth_y_less:
    fld dword ptr [esp]
    faddp st(6), st
depth_y_join:
    faddp st(2), st
    fild dword ptr [esp + 8]
    fmul dword ptr [factor_va]
    fild dword ptr [esp + 12]
    fmul dword ptr [factor_va]
    fstp dword ptr [esp]
    fcom dword ptr [esp]
    fnstsw ax
    test ah, 5
    jp depth_z_less
    faddp st(4), st
    fld dword ptr [esp]
    jmp depth_z_join
depth_z_less:
    fld dword ptr [esp]
    faddp st(5), st
depth_z_join:
    faddp st(3), st
    dec ecx
    jnz depth_loop
    call depth_observe
    fstp qword ptr [outputs_va]
    fstp qword ptr [outputs_va + 8]
    fstp qword ptr [outputs_va + 16]
    fstp qword ptr [outputs_va + 24]
    fstp qword ptr [outputs_va + 32]
    fstp qword ptr [outputs_va + 40]
    add esp, 4
    ret
.size x87_full_depth, .-x87_full_depth

.type depth_observe, @function
depth_observe:
    fnstsw word ptr [outputs_va + 48]
    fnstcw word ptr [outputs_va + 50]
    ret
.size depth_observe, .-depth_observe
