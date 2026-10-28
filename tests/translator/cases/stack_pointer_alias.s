# stack_pointer_alias -- an unknown pointer from data aliases stack slots; known
# stack stores must not be forwarded across the aliasing store, while global
# stores remain visible through the same interleaving.

.section .data
alias_pointer:
    .long ESP_INIT + 4
global_a:
    .long 0
global_b:
    .long 0
alias_global_pointer:
    .long DATA_BASE + (global_a - data_start)

.set alias_pointer_va, DATA_BASE + (alias_pointer - data_start)
.set global_a_va, DATA_BASE + (global_a - data_start)
.set global_b_va, DATA_BASE + (global_b - data_start)
.set alias_global_pointer_va, DATA_BASE + (alias_global_pointer - data_start)

.text
.globl stack_pointer_alias
.type stack_pointer_alias, @function
stack_pointer_alias:
    mov esi, [alias_pointer_va]
    mov dword ptr [esp + 4], 0x11111111
    mov dword ptr [global_a_va], 0xAAAAAAAA
    mov dword ptr [esi], 0xBBBBBBBB
    mov eax, [esp + 4]
    mov edx, [global_a_va]
    mov dword ptr [global_b_va], 0xCCCCCCCC
    mov dword ptr [esi + 4], 0xDDDDDDDD
    mov ecx, [esp + 8]
    mov ebx, [global_b_va]
    xor eax, edx
    xor ecx, ebx
    ret
.size stack_pointer_alias, .-stack_pointer_alias

.globl global_pointer_alias
.type global_pointer_alias, @function
global_pointer_alias:
    mov esi, [alias_global_pointer_va]
    mov dword ptr [global_a_va], 0xAAAAAAAA
    mov dword ptr [esi], 0xBBBBBBBB
    mov eax, [global_a_va]
    ret
.size global_pointer_alias, .-global_pointer_alias
