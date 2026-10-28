# call_indirect_tail -- calls through a register and a .data function pointer, a
# bounded recursive helper, and a tail jmp that returns through the caller's frame.

.section .data
fnptr:
    .long helper_add_va
.set fnptr_va, DATA_BASE + (fnptr - data_start)

.text
.set helper_add_va, TEXT_BASE + (helper_add - case_start)

.globl call_indirect_tail
.type call_indirect_tail, @function
call_indirect_tail:
    mov eax, offset helper_add_va
    push dword ptr [esp + 8]
    push dword ptr [esp + 8]
    call eax
    add esp, 8
    mov ecx, eax
    push dword ptr [esp + 8]
    push dword ptr [esp + 8]
    call dword ptr [fnptr_va]
    add esp, 8
    add eax, ecx
    mov edx, [esp + 8]
    and edx, 7
    push edx
    call helper_rec
    add esp, 4
    add eax, edx
    jmp helper_tail
.size call_indirect_tail, .-call_indirect_tail

.type helper_add, @function
helper_add:
    mov eax, [esp + 4]
    add eax, [esp + 8]
    ret
.size helper_add, .-helper_add

.type helper_rec, @function
helper_rec:
    mov eax, [esp + 4]
    test eax, eax
    jz helper_rec_zero
    dec eax
    push eax
    call helper_rec
    add esp, 4
    add eax, 3
    ret
helper_rec_zero:
    mov eax, 7
    ret
.size helper_rec, .-helper_rec

.type helper_tail, @function
helper_tail:
    mov edx, [esp + 4]
    sub edx, [esp + 8]
    add eax, edx
    ret
.size helper_tail, .-helper_tail

# input: arg0=0 arg1=0
# input: arg0=1 arg1=1
# input: arg0=0x80000000 arg1=0x7fffffff
# input: arg0=0 arg1=7
