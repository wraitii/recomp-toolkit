# mem_alias_rmw -- a runtime-computed pointer may or may not alias direct
# stores; read-modify-write forms and REP string moves with overlap and DF.
# input: arg0=0 arg1=0 arg2=0 arg3=0x11223344
# input: arg0=4 arg1=1 arg2=1 arg3=0
# input: arg0=8 arg1=0xffffffff arg2=0x80000000 arg3=0x11223344
# input: arg0=12 arg1=0x7fffffff arg2=0xffffffff arg3=0x55667788

.section .data
buf:
    .space 64
ptr_slot:
    .long 0
src:
    .byte 0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76, 0x87
    .byte 0x98, 0xA9, 0xBA, 0xCB, 0xDC, 0xED, 0xFE, 0x0F
dst:
    .space 32
ovl_back:
    .byte 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15
ovl_forward:
    .byte 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15
stos_buf:
    .long 0, 0, 0, 0
results:
    .space 96

.set buf_va, DATA_BASE + (buf - data_start)
.set ptr_slot_va, DATA_BASE + (ptr_slot - data_start)
.set src_va, DATA_BASE + (src - data_start)
.set dst_va, DATA_BASE + (dst - data_start)
.set ovl_back_va, DATA_BASE + (ovl_back - data_start)
.set ovl_forward_va, DATA_BASE + (ovl_forward - data_start)
.set stos_buf_va, DATA_BASE + (stos_buf - data_start)
.set results_va, DATA_BASE + (results - data_start)

.text
.globl mem_alias_rmw
.type mem_alias_rmw, @function
mem_alias_rmw:
    mov eax, [esp + 4]
    and eax, 12
    lea esi, [buf_va + eax]
    mov dword ptr [ptr_slot_va], esi
    mov edi, [ptr_slot_va]
    mov dword ptr [buf_va], 0x11111111
    mov dword ptr [buf_va + 4], 0x22222222
    mov dword ptr [buf_va + 8], 0x33333333
    mov dword ptr [buf_va + 12], 0x44444444
    mov dword ptr [edi], 0xAAAAAAAA
    mov ebx, [buf_va]
    mov ecx, [buf_va + 4]
    mov edx, [buf_va + 8]
    mov ebp, [buf_va + 12]
    mov dword ptr [results_va], ebx
    mov dword ptr [results_va + 4], ecx
    mov dword ptr [results_va + 8], edx
    mov dword ptr [results_va + 12], ebp

    mov dword ptr [buf_va + 12], 0x55555555
    mov dword ptr [buf_va + 12], 0x66666666
    mov dword ptr [edi], 0xBBBBBBBB
    mov eax, [buf_va + 12]
    mov dword ptr [results_va + 16], eax

    mov dword ptr [buf_va + 24], 0x01020304
    mov esi, [esp + 8]
    add dword ptr [buf_va + 24], esi
    mov ecx, [buf_va + 24]
    mov byte ptr [buf_va + 28], 0xFE
    inc byte ptr [buf_va + 28]
    movzx edx, byte ptr [buf_va + 28]
    mov byte ptr [buf_va + 29], 0xFF
    inc byte ptr [buf_va + 29]
    movzx ebp, byte ptr [buf_va + 29]
    mov dword ptr [results_va + 20], ecx
    mov dword ptr [results_va + 24], edx
    mov dword ptr [results_va + 28], ebp

    mov eax, 0x12345678
    mov dword ptr [buf_va + 32], 0x87654321
    xchg dword ptr [buf_va + 32], eax
    mov dword ptr [results_va + 32], eax
    mov ecx, [buf_va + 32]
    mov eax, [esp + 12]
    mov dword ptr [buf_va + 36], 0x01010101
    lock xadd dword ptr [buf_va + 36], eax
    mov edx, [buf_va + 36]
    mov dword ptr [results_va + 36], ecx
    mov dword ptr [results_va + 40], eax
    mov dword ptr [results_va + 44], edx

    mov dword ptr [buf_va + 40], 0x11223344
    mov eax, [esp + 16]
    lea ecx, [eax + 1]
    mov edx, 0x55667788
    lock cmpxchg dword ptr [buf_va + 40], edx
    mov ebx, [buf_va + 40]
    mov dword ptr [results_va + 48], ebx

    mov dword ptr [buf_va + 44], 0
    mov ecx, 5
mem_alias_loop:
    add dword ptr [buf_va + 44], ecx
    dec ecx
    jnz mem_alias_loop
    mov esi, [buf_va + 44]
    mov dword ptr [results_va + 52], esi

    lea esi, [src_va]
    lea edi, [dst_va]
    mov ecx, 12
    rep movsb
    mov eax, [dst_va]
    mov ebx, [dst_va + 8]
    mov dword ptr [results_va + 56], eax
    mov dword ptr [results_va + 60], ebx

    std
    lea esi, [ovl_back_va + 4]
    lea edi, [ovl_back_va + 6]
    mov ecx, 6
    rep movsb
    cld
    mov ebx, [ovl_back_va]
    mov ecx, [ovl_back_va + 4]
    mov edx, [ovl_back_va + 8]
    mov ebp, [ovl_back_va + 12]
    mov dword ptr [results_va + 64], ebx
    mov dword ptr [results_va + 68], ecx
    mov dword ptr [results_va + 72], edx
    mov dword ptr [results_va + 76], ebp

    lea esi, [ovl_forward_va]
    lea edi, [ovl_forward_va + 2]
    mov ecx, 6
    rep movsb
    mov eax, [ovl_forward_va]
    mov dword ptr [results_va + 80], eax

    mov dword ptr [stos_buf_va], 0xCAFEBABE
    lea edi, [stos_buf_va]
    mov eax, 0xFFFFFFFF
    xor ecx, ecx
    rep stosd
    mov ebx, [stos_buf_va]
    lea edi, [stos_buf_va + 4]
    mov eax, 0x0BADF00D
    mov ecx, 3
    rep stosd
    mov ecx, [stos_buf_va + 8]
    mov edx, edi
    ret
.size mem_alias_rmw, .-mem_alias_rmw
