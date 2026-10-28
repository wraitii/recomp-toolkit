# basic -- integer args, arithmetic, setcc, a conditional branch and ret N stdcall cleanup.
# input: arg0=0 arg1=0 arg2=1
# input: arg0=0x7fffffff arg1=1 arg2=0
# input: arg0=0x80000000 arg1=0xffffffff arg2=0x80000000
# input: arg0=7 arg1=11 arg2=18

.globl basic
.type basic, @function
basic:
    push ebp
    mov ebp, esp
    mov eax, [ebp + 8]
    mov ecx, [ebp + 12]
    add eax, ecx
    cmp eax, [ebp + 16]
    setg dl
    movzx edx, dl
    test edx, edx
    jz basic_zero
    imul eax, eax, 3
    jmp basic_done
basic_zero:
    sub eax, ecx
basic_done:
    add eax, edx
    pop ebp
    ret 12
.size basic, .-basic
