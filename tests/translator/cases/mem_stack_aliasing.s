# mem_stack_aliasing -- ESP/EBP aliases of argument slots after SUB/PUSH,
# including push/pop forms whose value or address uses the changed ESP.
# input: arg0=0x11111111 arg1=0x22222222 arg2=0x33333333

.section .data
results:
    .space 16

.set results_va, DATA_BASE + (results - data_start)

.text
.globl mem_stack_aliasing
.type mem_stack_aliasing, @function
mem_stack_aliasing:
    mov ebp, esp
    mov dword ptr [ebp + 8], 0x13572468
    mov eax, [esp + 8]
    sub esp, 0x20
    mov dword ptr [esp + 0x24], 0x24681357
    mov ecx, [ebp + 4]
    mov dword ptr [ebp + 12], 0xABCDEF01
    mov edx, [esp + 0x2c]
    add esp, 0x20
    mov dword ptr [results_va], eax
    mov dword ptr [results_va + 4], ecx
    mov dword ptr [results_va + 8], edx

    push 0x11111111
    mov dword ptr [esp], 0x22222222
    pop ebx
    mov esi, [esp + 4]
    mov dword ptr [results_va + 12], esi
    push dword ptr [esp + 4]
    pop edi
    push esp
    pop eax

    mov esp, ebp
    add esp, 8
    push 0x33333333
    pop dword ptr [esp]
    mov ecx, [esp]
    mov esp, ebp

    lea esi, [esp + 0x80]
    push esi
    pop esp
    mov edx, [esp]
    mov esp, ebp
    ret
.size mem_stack_aliasing, .-mem_stack_aliasing
