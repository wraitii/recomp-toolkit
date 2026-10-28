# flag_descriptors -- INC preserving CF through a carry branch and lazy descriptor,
# a CF flag join at a diamond, an AH read from an unwritten high lane, and SHL OF.
# input: arg0=0 arg1=0
# input: arg0=1 arg1=1
# input: arg0=0x80000000 arg1=1
# input: arg0=0xffffffff arg1=2
# input: arg0=0x7fffffff arg1=0

.text
.globl inc_preserves_cf
.type inc_preserves_cf, @function
inc_preserves_cf:
    mov eax, [esp + 4]
    stc
    inc eax
    jc inc_cf_set
    mov eax, 0x1111
    ret
inc_cf_set:
    mov eax, 0x2222
    ret
.size inc_preserves_cf, .-inc_preserves_cf

.globl flag_join_cf
.type flag_join_cf, @function
flag_join_cf:
    mov eax, [esp + 4]
    mov edx, [esp + 8]
    test edx, edx
    jz flag_join_add
    sub eax, 1
    jmp flag_join_merge
flag_join_add:
    add eax, 1
flag_join_merge:
    jc flag_join_carry
    mov eax, 0x3333
    ret
flag_join_carry:
    mov eax, 0x4444
    ret
.size flag_join_cf, .-flag_join_cf

.globl ah_input
.type ah_input, @function
ah_input:
    mov eax, [esp + 4]
    movzx edx, ah
    movzx ecx, al
    add eax, edx
    xor eax, ecx
    ret
.size ah_input, .-ah_input

.globl shl_overflow
.type shl_overflow, @function
shl_overflow:
    mov eax, [esp + 4]
    mov ecx, [esp + 8]
    shl eax, cl
    jo shl_of_set
    mov eax, 0x5555
    ret
shl_of_set:
    mov eax, 0x6666
    ret
.size shl_overflow, .-shl_overflow
