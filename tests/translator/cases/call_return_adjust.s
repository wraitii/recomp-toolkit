# A callee moves its return address forward; the caller resumes at the adjusted boundary.

.text
.globl call_return_adjust
.type call_return_adjust, @function
call_return_adjust:
    mov ecx, [esp + 4]
    push ecx
    call skip_increment
    add eax, 0x12345678
    pop ecx
    add eax, ecx
    ret
.size call_return_adjust, .-call_return_adjust

.type skip_increment, @function
skip_increment:
    mov eax, [esp + 4]
    add dword ptr [esp], 5
    ret
.size skip_increment, .-skip_increment

# input: arg0=0
# input: arg0=1
# input: arg0=0xffffffff

.globl call_return_adjust_dynamic
.type call_return_adjust_dynamic, @function
call_return_adjust_dynamic:
    xor eax, eax
    call dynamic_adjust
    add eax, 0x11111111
    ret
.size call_return_adjust_dynamic, .-call_return_adjust_dynamic

.type dynamic_adjust, @function
dynamic_adjust:
    add dword ptr [esp], eax
    ret
.size dynamic_adjust, .-dynamic_adjust

.globl call_return_adjust_twice
.type call_return_adjust_twice, @function
call_return_adjust_twice:
    mov ecx, [esp + 4]
    push ecx
    call skip_two
    add eax, 0x01010101
    add eax, 0x02020202
    xor eax, 0x5a5a5a5a
    call skip_increment
    add eax, 0x03030303
    pop ecx
    sub eax, ecx
    ret
.size call_return_adjust_twice, .-call_return_adjust_twice

.type skip_two, @function
skip_two:
    mov eax, [esp + 4]
    add dword ptr [esp], 10
    ret
.size skip_two, .-skip_two
