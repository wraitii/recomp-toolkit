# translate: assume = ["private-stack"]
# input: arg0=0x12345678 esi=0x87654321
# input: arg0=0 esi=0xffffffff

.section .data
fallback_remaining:
    .long 0
fallback_result:
    .long 0
.set fallback_remaining_va, DATA_BASE + (fallback_remaining - data_start)
.set fallback_result_va, DATA_BASE + (fallback_result - data_start)

.text
.globl stack_call_fallback
.type stack_call_fallback, @function
stack_call_fallback:
    mov ecx, [esp + 4]
    mov dword ptr [fallback_remaining_va], 1024
    mov dword ptr [fallback_result_va], 0
    call fallback_leaf
fallback_loop:
    call fallback_preserved
    xor dword ptr [fallback_result_va], eax
    dec dword ptr [fallback_remaining_va]
    jnz fallback_loop
    mov eax, [fallback_result_va]
    ret
.size stack_call_fallback, .-stack_call_fallback

.type fallback_preserved, @function
fallback_preserved:
    push esi
    mov esi, ecx
    call fallback_leaf
    mov eax, esi
    pop esi
    ret
.size fallback_preserved, .-fallback_preserved

.type fallback_leaf, @function
fallback_leaf:
    mov eax, 0x13579bdf
    ret
.size fallback_leaf, .-fallback_leaf
