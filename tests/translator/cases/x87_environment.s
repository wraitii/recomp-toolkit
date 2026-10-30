# input: arg0=1 arg1=3
# input: arg0=3 arg1=1
# input: arg0=2 arg1=2
# input: arg0=0xfffffffd arg1=4

.section .data
environment_out: .zero 64
.set environment_out_va, DATA_BASE + (environment_out - data_start)

.text
.globl x87_environment
.type x87_environment, @function
x87_environment:
    sub esp, 28
    fld1
    fldz
    xor esi, esi
    mov ecx, 11
environment_loop:
    fild dword ptr [esp + 32]
    fild dword ptr [esp + 36]
    fcomp st(1)
    fnstsw ax
    fstp st(0)
    test ah, 5
    jp environment_join
    mov esi, 1
environment_join:
    dec ecx
    jnz environment_loop
    mov dword ptr [environment_out_va], esi
    call environment_observe
    fld1
    fxam
    fnstsw word ptr [environment_out_va + 10]
    fstp st(0)
    fnstenv [esp]
    mov ax, word ptr [esp]
    mov word ptr [environment_out_va + 12], ax
    mov ax, word ptr [esp + 4]
    mov word ptr [environment_out_va + 14], ax
    mov ax, word ptr [esp + 8]
    mov word ptr [environment_out_va + 16], ax
    fldenv [esp]
    mov dword ptr [esp], 0
    mov dword ptr [esp + 4], 0
    mov dword ptr [esp + 8], 0
    mov dword ptr [esp + 12], 0
    mov dword ptr [esp + 16], 0
    mov dword ptr [esp + 20], 0
    mov dword ptr [esp + 24], 0
    fldz
    fstp qword ptr [environment_out_va + 24]
    fnstsw word ptr [environment_out_va + 32]
    fstp st(0)
    fstp st(0)
    add esp, 28
    ret
.size x87_environment, .-x87_environment

.type environment_observe, @function
environment_observe:
    fnstsw word ptr [environment_out_va + 4]
    fnstcw word ptr [environment_out_va + 6]
    fld1
    fnstsw word ptr [environment_out_va + 8]
    fstp st(0)
    ret
.size environment_observe, .-environment_observe

.globl x87_environment_helpers
.type x87_environment_helpers, @function
x87_environment_helpers:
    fld1
    fldz
    clc
    fcmovb st(0), st(1)
    fstp qword ptr [environment_out_va]
    fstp st(0)
    fld1
    fldz
    stc
    fcmovb st(0), st(1)
    fstp qword ptr [environment_out_va + 8]
    fstp st(0)
    fldz
    fptan
    fstp qword ptr [environment_out_va + 16]
    fstp qword ptr [environment_out_va + 24]
    fldz
    fsincos
    fstp qword ptr [environment_out_va + 32]
    fstp qword ptr [environment_out_va + 40]
    fld1
    emms
    fxam
    fnstsw word ptr [environment_out_va + 48]
    ret
.size x87_environment_helpers, .-x87_environment_helpers

.globl x87_stack_branch
.type x87_stack_branch, @function
x87_stack_branch:
    xor esi, esi
    mov ecx, 7
stack_branch_loop:
    fild dword ptr [esp + 4]
    fild dword ptr [esp + 8]
    fld st(0)
    fadd st(0), st(2)
    fcomp dword ptr [environment_out_va + 60]
    fnstsw ax
    test ah, 5
    jnp stack_branch_outside
    fsub st(0), st(1)
    fcomp dword ptr [environment_out_va + 60]
    fnstsw ax
    fstp st(0)
    test ah, 5
    jp stack_branch_join
    mov esi, 1
stack_branch_join:
    dec ecx
    jnz stack_branch_loop
    mov dword ptr [environment_out_va], esi
    fnstsw word ptr [environment_out_va + 4]
    ret
stack_branch_outside:
    fstp st(0)
    fstp st(0)
    mov dword ptr [environment_out_va], 2
    fnstsw word ptr [environment_out_va + 4]
    ret
.size x87_stack_branch, .-x87_stack_branch
