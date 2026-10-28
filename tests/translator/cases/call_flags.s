# call_flags -- a callee reads the caller's CF and returns its own CF; the caller consumes both.

.text
.globl call_flags
.type call_flags, @function
call_flags:
    mov eax, [esp + 4]
    mov edx, [esp + 8]
    cmp eax, edx
    call capture_carry
    sbb edx, edx
    sub eax, edx
    xor eax, ecx
    ret
.size call_flags, .-call_flags

.type capture_carry, @function
capture_carry:
    sbb ecx, ecx
    mov eax, [esp + 4]
    add eax, [esp + 8]
    ret
.size capture_carry, .-capture_carry

# input: arg0=0 arg1=0
# input: arg0=0xffffffff arg1=1 eflags=0x2
# input: arg0=0x80000000 arg1=0x7fffffff
# input: arg0=1 arg1=0xffffffff eflags=0x202
