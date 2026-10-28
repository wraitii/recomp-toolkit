# Intra-function CALL: the pushed return address is kept and the transfer stays local.

.text
.globl call_intra_fallthrough
.type call_intra_fallthrough, @function
call_intra_fallthrough:
    call geteip_next
geteip_next:
    pop eax
    add eax, [esp + 4]
    ret
.size call_intra_fallthrough, .-call_intra_fallthrough

.globl call_intra_subroutine
.type call_intra_subroutine, @function
call_intra_subroutine:
    push ebx
    xor ebx, ebx
    call local_sub
    add eax, 0x11111111
    pop ebx
    ret
local_sub:
    add eax, 0x22222222
    ret
.size call_intra_subroutine, .-call_intra_subroutine

# input: arg0=0
# input: arg0=1
# input: arg0=0xffffffff
