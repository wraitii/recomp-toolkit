# translate: stack_memory = true
# input: arg0=0 arg1=0
# input: arg0=1 arg1=3
# input: arg0=0xffffffff arg1=7

.section .data
range_static:
    .space 32
.set range_static_va, DATA_BASE + (range_static - data_start)

.text
.globl range_call_frames
.type range_call_frames, @function
range_call_frames:
    push ebp
    mov ebp, esp
    sub esp, 32
    mov eax, [ebp + 8]
    mov dword ptr [ebp - 4], eax
    mov dword ptr [ebp - 8], 0x12345678
    push eax
    call range_write_arg
    mov ecx, [ebp - 4]
    xor ecx, [esp]
    add esp, 4
    sub esp, 8
    push ecx
    call range_write_arg
    mov edx, [esp]
    add esp, 12
    mov eax, [ebp - 4]
    add eax, [ebp - 8]
    xor eax, edx
    mov esp, ebp
    pop ebp
    ret
.size range_call_frames, .-range_call_frames

.type range_write_arg, @function
range_write_arg:
    xor dword ptr [esp + 4], 0xa5a5a5a5
    mov eax, [esp + 4]
    ret
.size range_write_arg, .-range_write_arg

.globl range_opaque_stack
.type range_opaque_stack, @function
range_opaque_stack:
    sub esp, 16
    mov dword ptr [esp], 0x13579bdf
    lea ecx, [esp]
    mov [esp + 4], ecx
    mov edx, [esp + 4]
    call range_pointer_write
    mov eax, [esp]
    add esp, 16
    ret
.size range_opaque_stack, .-range_opaque_stack

.type range_pointer_write, @function
range_pointer_write:
    mov byte ptr [edx + 1], 0x42
    ret
.size range_pointer_write, .-range_pointer_write

.globl range_bounded_index
.type range_bounded_index, @function
range_bounded_index:
    sub esp, 64
    mov dword ptr [esp], 0xabcdef01
    mov dword ptr [esp + 32], 0x12345678
    mov ecx, [esp + 68]
    and ecx, 7
    mov dword ptr [esp + ecx * 4 + 4], 0x11223344
    mov eax, [esp]
    xor eax, [esp + 32]
    add esp, 64
    ret
.size range_bounded_index, .-range_bounded_index

.globl range_phi_overlap
.type range_phi_overlap, @function
range_phi_overlap:
    sub esp, 16
    mov dword ptr [esp], 0x11223344
    mov dword ptr [esp + 4], 0x55667788
    test byte ptr [esp + 20], 1
    jz range_phi_join
    mov word ptr [esp + 3], 0xaabb
range_phi_join:
    mov eax, [esp]
    xor eax, [esp + 4]
    add esp, 16
    ret
.size range_phi_overlap, .-range_phi_overlap

.globl range_static_call
.type range_static_call, @function
range_static_call:
    mov dword ptr [range_static_va], 0x12345678
    call range_static_write
    mov eax, [range_static_va]
    xor eax, [range_static_va + 4]
    ret
.size range_static_call, .-range_static_call

.type range_static_write, @function
range_static_write:
    mov dword ptr [range_static_va + 4], 0xabcdef01
    ret
.size range_static_write, .-range_static_write

.globl range_recursive
.type range_recursive, @function
range_recursive:
    mov ecx, [esp + 4]
    and ecx, 3
    push ecx
    call range_recurse
    add esp, 4
    mov eax, [range_static_va + 8]
    ret
.size range_recursive, .-range_recursive

.type range_recurse, @function
range_recurse:
    mov eax, [esp + 4]
    mov dword ptr [range_static_va + 8], eax
    test eax, eax
    jz range_recurse_done
    dec eax
    push eax
    call range_recurse
    add esp, 4
range_recurse_done:
    ret
.size range_recurse, .-range_recurse

.globl range_opaque_access
.type range_opaque_access, @function
range_opaque_access:
    sub esp, 16
    mov dword ptr [esp], 0x11223344
    mov dword ptr [esp + 4], 0x55667788
    lea ecx, [esp]
    test byte ptr [esp + 20], 1
    jz range_opaque_join
    add ecx, 4
range_opaque_join:
    mov eax, [ecx]
    mov byte ptr [ecx + 1], 0xaa
    xor eax, [esp]
    xor eax, [esp + 4]
    add esp, 16
    ret
.size range_opaque_access, .-range_opaque_access
