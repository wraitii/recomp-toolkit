# cf_loops -- nested loops with break/continue, flags from the final iteration
# consumed after exit through non-flag ops, and two-entry goto CFG.

# input: arg0=0 arg1=0
# input: arg0=1 arg1=1
# input: arg0=2 arg1=3
# input: arg0=7 arg1=7
# input: arg0=0xffffffff arg1=0x80000000

.text
.globl cf_nested
.type cf_nested, @function
cf_nested:
    push ebx
    push esi
    push edi
    mov ebx, [esp + 16]
    and ebx, 7
    inc ebx
    mov esi, [esp + 20]
    and esi, 7
    inc esi
    xor edi, edi
cf_nested_outer:
    xor ecx, ecx
cf_nested_inner:
    inc ecx
    test ecx, 1
    jz cf_nested_even
    add edi, ecx
    jmp cf_nested_next
cf_nested_even:
    cmp ecx, 4
    je cf_nested_after_inner
    add edi, 0x10
cf_nested_next:
    cmp ecx, esi
    jl cf_nested_inner
cf_nested_after_inner:
    dec ebx
    jnz cf_nested_outer
    jz cf_nested_zero
    mov edi, 0xdead
cf_nested_zero:
    mov eax, edi
    pop edi
    pop esi
    pop ebx
    ret
.size cf_nested, .-cf_nested

.globl cf_loop_flags
.type cf_loop_flags, @function
cf_loop_flags:
    mov ecx, [esp + 4]
    and ecx, 7
    xor eax, eax
cf_loop_flags_again:
    add eax, 3
    dec ecx
    jns cf_loop_flags_again
    mov edx, eax
    lea edx, [edx + ecx*2]
    push edx
    pop edx
    js cf_loop_flags_negative
    mov eax, 0x777
    jmp cf_loop_flags_out
cf_loop_flags_negative:
    mov eax, edx
    add eax, 0x100
cf_loop_flags_out:
    ret
.size cf_loop_flags, .-cf_loop_flags

.globl cf_irreducible
.type cf_irreducible, @function
cf_irreducible:
    mov ecx, [esp + 4]
    and ecx, 7
    add ecx, 4
    xor edx, edx
    test ecx, 1
    jz cf_irr_entry_b
cf_irr_entry_a:
    inc edx
cf_irr_entry_b:
    add edx, 2
    cmp edx, ecx
    jl cf_irr_entry_a
    mov eax, edx
    ret
.size cf_irreducible, .-cf_irreducible
