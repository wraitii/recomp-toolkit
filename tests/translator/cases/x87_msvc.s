# x87_msvc -- MSVC compare idioms (fnstsw ax + test/and ah masks) on input-driven values,
# an _ftol-style truncating control-word swap, and x87 values live across calls.
# input: arg0=0 arg1=0
# input: arg0=3 arg1=3
# input: arg0=5 arg1=0xfffffffb
# input: arg0=0x80000000 arg1=0x7fffffff
# input: arg0=0xffffffff arg1=1

.section .data
half:
    .quad 0x3fe0000000000000
msvc_out:
    .space 64
.set half_va, DATA_BASE + (half - data_start)
.set msvc_out_va, DATA_BASE + (msvc_out - data_start)

.text
.globl x87_msvc
.type x87_msvc, @function
x87_msvc:
    push ebx
    xor ebx, ebx
    fild dword ptr [esp + 8]
    fild dword ptr [esp + 12]

    fcom st(1)
    fnstsw ax
    test ah, 0x41
    jnz msvc_not_greater
    or ebx, 1
msvc_not_greater:
    fcom st(1)
    fnstsw ax
    test ah, 0x05
    jp msvc_not_less_equal
    or ebx, 2
msvc_not_less_equal:
    fcom st(1)
    fnstsw ax
    test ah, 0x44
    jp msvc_not_equal
    or ebx, 4
msvc_not_equal:
    fcom st(1)
    fnstsw ax
    and ah, 0x45
    cmp ah, 0x01
    jne msvc_not_below
    or ebx, 8
msvc_not_below:
    fcomp st(1)
    fnstsw ax
    test ah, 0x01
    setnz cl
    movzx ecx, cl
    shl ecx, 4
    or ebx, ecx
    mov dword ptr [msvc_out_va], ebx

    fmul qword ptr [half_va]
    fld st(0)
    call msvc_ftol
    mov dword ptr [msvc_out_va + 4], eax
    mov dword ptr [msvc_out_va + 8], edx
    fld st(0)
    fistp qword ptr [msvc_out_va + 12]

    call msvc_bump
    fadd st(0), st(0)
    fstp qword ptr [msvc_out_va + 20]

    mov eax, ebx
    pop ebx
    ret
.size x87_msvc, .-x87_msvc

.type msvc_ftol, @function
msvc_ftol:
    push ebp
    mov ebp, esp
    sub esp, 12
    fnstcw word ptr [ebp - 2]
    mov ax, word ptr [ebp - 2]
    or ah, 0x0c
    mov word ptr [ebp - 4], ax
    fldcw word ptr [ebp - 4]
    fistp qword ptr [ebp - 12]
    fldcw word ptr [ebp - 2]
    mov eax, [ebp - 12]
    mov edx, [ebp - 8]
    leave
    ret
.size msvc_ftol, .-msvc_ftol

.type msvc_bump, @function
msvc_bump:
    fld1
    fadd st(0), st(1)
    fld1
    fsubp st(1), st(0)
    fstp st(1)
    fld1
    faddp st(1), st(0)
    ret
.size msvc_bump, .-msvc_bump
