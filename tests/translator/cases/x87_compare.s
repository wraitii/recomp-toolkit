# x87_compare -- fcom/fcomp/fcompp/fucompp status words through fnstsw+sahf,
# setcc and branch patterns, plus fcomi/fcomip flag results including unordered.

.section .data
pairs:
    .quad 0x4000000000000000, 0x3ff0000000000000
    .quad 0x3ff0000000000000, 0x4000000000000000
    .quad 0x4000000000000000, 0x4000000000000000
    .quad 0x0000000000000000, 0x8000000000000000
    .quad 0x7ff8000000000000, 0x3ff0000000000000
    .quad 0x3ff0000000000000, 0x7ff8000000000000
    .quad 0xc014000000000000, 0x4008000000000000
one_v: .quad 0x3ff0000000000000
two_v: .quad 0x4000000000000000
nan_v: .quad 0x7ff8000000000000
sw_out: .zero 32
branch_out: .zero 32
flags_out: .zero 64
compp_out: .zero 8

.set pairs_va, DATA_BASE + (pairs - data_start)
.set one_v_va, DATA_BASE + (one_v - data_start)
.set two_v_va, DATA_BASE + (two_v - data_start)
.set nan_v_va, DATA_BASE + (nan_v - data_start)
.set sw_out_va, DATA_BASE + (sw_out - data_start)
.set branch_out_va, DATA_BASE + (branch_out - data_start)
.set flags_out_va, DATA_BASE + (flags_out - data_start)
.set compp_out_va, DATA_BASE + (compp_out - data_start)

.text
.globl x87_compare_sw
.type x87_compare_sw, @function
x87_compare_sw:
    lea esi, [pairs_va]
    lea edi, [sw_out_va]
    mov ecx, 7
sw_loop:
    fld qword ptr [esi]
    fcom qword ptr [esi + 8]
    fnstsw ax
    sahf
    seta byte ptr [edi]
    setb byte ptr [edi + 1]
    sete byte ptr [edi + 2]
    setp byte ptr [edi + 3]
    fstp st(0)
    add esi, 16
    add edi, 4
    dec ecx
    jnz sw_loop
    xor eax, eax
    ret
.size x87_compare_sw, .-x87_compare_sw

.globl x87_compare_branch
.type x87_compare_branch, @function
x87_compare_branch:
    lea esi, [pairs_va]
    lea edi, [branch_out_va]
    mov ecx, 7
branch_loop:
    fld qword ptr [esi]
    fcomp qword ptr [esi + 8]
    fnstsw ax
    sahf
    jp branch_unordered
    ja branch_greater
    jb branch_less
    mov eax, 2
    jmp branch_store
branch_greater:
    mov eax, 0
    jmp branch_store
branch_less:
    mov eax, 1
    jmp branch_store
branch_unordered:
    mov eax, 3
branch_store:
    mov dword ptr [edi], eax
    add esi, 16
    add edi, 4
    dec ecx
    jnz branch_loop
    xor eax, eax
    ret
.size x87_compare_branch, .-x87_compare_branch

.globl x87_compare_flags
.type x87_compare_flags, @function
x87_compare_flags:
    lea esi, [pairs_va]
    lea edi, [flags_out_va]
    mov ecx, 7
fcomip_loop:
    fld qword ptr [esi]
    fld qword ptr [esi + 8]
    fcomip st, st(1)
    seta byte ptr [edi]
    setb byte ptr [edi + 1]
    sete byte ptr [edi + 2]
    setp byte ptr [edi + 3]
    fstp st(0)
    add esi, 16
    add edi, 4
    dec ecx
    jnz fcomip_loop
    lea esi, [pairs_va]
    lea edi, [flags_out_va + 32]
    mov ecx, 7
fcomi_loop:
    fld qword ptr [esi]
    fld qword ptr [esi + 8]
    fcomi st, st(1)
    seta byte ptr [edi]
    setb byte ptr [edi + 1]
    sete byte ptr [edi + 2]
    setp byte ptr [edi + 3]
    fstp st(0)
    fstp st(0)
    add esi, 16
    add edi, 4
    dec ecx
    jnz fcomi_loop
    xor eax, eax
    ret
.size x87_compare_flags, .-x87_compare_flags

.globl x87_compare_compp
.type x87_compare_compp, @function
x87_compare_compp:
    fld qword ptr [one_v_va]
    fld qword ptr [nan_v_va]
    fucompp
    fnstsw ax
    sahf
    setp byte ptr [compp_out_va]
    setb byte ptr [compp_out_va + 1]
    fld qword ptr [two_v_va]
    fld qword ptr [one_v_va]
    fcompp
    fnstsw ax
    sahf
    setb byte ptr [compp_out_va + 2]
    sete byte ptr [compp_out_va + 3]
    xor eax, eax
    ret
.size x87_compare_compp, .-x87_compare_compp
