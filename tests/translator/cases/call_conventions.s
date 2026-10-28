# call_conventions -- stdcall ret N, cdecl caller cleanup, nonstandard return registers,
# deliberate callee-saved clobbers, and a callee writing the caller's frame through a pointer.

.text
.globl call_conventions
.type call_conventions, @function
call_conventions:
    push ebx
    push esi
    push edi
    push dword ptr [esp + 20]
    push dword ptr [esp + 20]
    call std_add
    push dword ptr [esp + 20]
    push dword ptr [esp + 20]
    call cdecl_mul
    add esp, 8
    push dword ptr [esp + 20]
    push dword ptr [esp + 20]
    call clobber_saved
    add esp, 8
    add eax, ecx
    xor eax, edx
    add eax, ebx
    add eax, esi
    add eax, edi
    pop edi
    pop esi
    pop ebx
    ret
.size call_conventions, .-call_conventions

.type std_add, @function
std_add:
    mov eax, [esp + 4]
    add eax, [esp + 8]
    ret 8
.size std_add, .-std_add

.type cdecl_mul, @function
cdecl_mul:
    mov ecx, [esp + 4]
    imul ecx, [esp + 8]
    ret
.size cdecl_mul, .-cdecl_mul

.type clobber_saved, @function
clobber_saved:
    mov eax, [esp + 4]
    mov edx, [esp + 8]
    mov ebx, eax
    add ebx, 0x12345678
    mov esi, edx
    xor esi, 0x9abcdef0
    mov edi, eax
    imul edi, edx
    ret
.size clobber_saved, .-clobber_saved

.globl call_stack_pointer
.type call_stack_pointer, @function
call_stack_pointer:
    sub esp, 8
    mov dword ptr [esp], 0x11111111
    mov dword ptr [esp + 4], 0x22222222
    lea eax, [esp]
    push eax
    call write_pair
    add esp, 4
    mov eax, [esp]
    mov edx, [esp + 4]
    add esp, 8
    add eax, edx
    ret
.size call_stack_pointer, .-call_stack_pointer

.type write_pair, @function
write_pair:
    mov eax, [esp + 4]
    mov ecx, 0x33333333
    mov [eax], ecx
    mov ecx, 0x44444444
    mov [eax + 4], ecx
    ret
.size write_pair, .-write_pair

# input: arg0=0 arg1=0
# input: arg0=1 arg1=1
# input: arg0=0x7fffffff arg1=3
# input: arg0=0xffffffff arg1=0xffffffff
