# insn_mul_misc -- imul forms, mul/div/idiv widths, sign extension, xlat,
# lahf/sahf, all setcc and cmovcc conditions, loop family and lea wraparound.
# input: arg0=0 arg1=0 arg2=0 arg3=0
# input: arg0=0xffffffff arg1=0x80000000 arg2=0xffffffff arg3=0x7fffffff
# input: arg0=0x80000000 arg1=0xffffffff arg2=0x12345678 arg3=0xffffff80
# input: arg0=0x7fffffff arg1=1 arg2=0x10000 arg3=0x00008000
# input: arg0=0x12345680 arg1=0x9abcdef0 arg2=0x8001 arg3=0xffff

.section .data
r_imul1:
    .zero 8
r_imul2:
    .zero 4
r_imul3:
    .zero 4
r_mul32:
    .zero 8
r_div32:
    .zero 8
r_idiv32:
    .zero 8
r_div16:
    .zero 8
r_idiv16:
    .zero 8
r_mul8:
    .zero 4
r_div8:
    .zero 4
r_idiv8:
    .zero 4
r_cdq:
    .zero 8
r_cwde:
    .zero 4
r_cbw:
    .zero 4
r_cwd:
    .zero 8
r_movsx8:
    .zero 4
r_movsx16:
    .zero 4
r_movzx8:
    .zero 4
r_movzx16:
    .zero 4
r_xlat:
    .zero 4
r_lahf:
    .zero 4
r_sahf:
    .zero 4
r_loop:
    .zero 4
r_loope:
    .zero 4
r_loopne:
    .zero 4
r_jecxz:
    .zero 4
r_lea1:
    .zero 4
r_lea2:
    .zero 4
r_lea3:
    .zero 4
r_lea4:
    .zero 4
setcc_buf:
    .zero 16
cmov_vals:
    .long 0x11111111, 0x22222222, 0x33333333, 0x44444444, 0x55555555, 0x66666666
    .long 0x77777777, 0x88888888, 0x99999999, 0xaaaaaaaa, 0xbbbbbbbb, 0xcccccccc
xlat_table:
    .byte 0x13, 0x27, 0x3b, 0x4f, 0x5a, 0x6e, 0x72, 0x86
    .byte 0x91, 0xa5, 0xb9, 0xcd, 0xd2, 0xe6, 0xfa, 0x0f

.set r_imul1_va, DATA_BASE + (r_imul1 - data_start)
.set r_imul2_va, DATA_BASE + (r_imul2 - data_start)
.set r_imul3_va, DATA_BASE + (r_imul3 - data_start)
.set r_mul32_va, DATA_BASE + (r_mul32 - data_start)
.set r_div32_va, DATA_BASE + (r_div32 - data_start)
.set r_idiv32_va, DATA_BASE + (r_idiv32 - data_start)
.set r_div16_va, DATA_BASE + (r_div16 - data_start)
.set r_idiv16_va, DATA_BASE + (r_idiv16 - data_start)
.set r_mul8_va, DATA_BASE + (r_mul8 - data_start)
.set r_div8_va, DATA_BASE + (r_div8 - data_start)
.set r_idiv8_va, DATA_BASE + (r_idiv8 - data_start)
.set r_cdq_va, DATA_BASE + (r_cdq - data_start)
.set r_cwde_va, DATA_BASE + (r_cwde - data_start)
.set r_cbw_va, DATA_BASE + (r_cbw - data_start)
.set r_cwd_va, DATA_BASE + (r_cwd - data_start)
.set r_movsx8_va, DATA_BASE + (r_movsx8 - data_start)
.set r_movsx16_va, DATA_BASE + (r_movsx16 - data_start)
.set r_movzx8_va, DATA_BASE + (r_movzx8 - data_start)
.set r_movzx16_va, DATA_BASE + (r_movzx16 - data_start)
.set r_xlat_va, DATA_BASE + (r_xlat - data_start)
.set r_lahf_va, DATA_BASE + (r_lahf - data_start)
.set r_sahf_va, DATA_BASE + (r_sahf - data_start)
.set r_loop_va, DATA_BASE + (r_loop - data_start)
.set r_loope_va, DATA_BASE + (r_loope - data_start)
.set r_loopne_va, DATA_BASE + (r_loopne - data_start)
.set r_jecxz_va, DATA_BASE + (r_jecxz - data_start)
.set r_lea1_va, DATA_BASE + (r_lea1 - data_start)
.set r_lea2_va, DATA_BASE + (r_lea2 - data_start)
.set r_lea3_va, DATA_BASE + (r_lea3 - data_start)
.set r_lea4_va, DATA_BASE + (r_lea4 - data_start)
.set setcc_buf_va, DATA_BASE + (setcc_buf - data_start)
.set cmov_vals_va, DATA_BASE + (cmov_vals - data_start)
.set xlat_table_va, DATA_BASE + (xlat_table - data_start)

.text
.globl mul_div
.type mul_div, @function
mul_div:
    mov eax, [esp + 4]
    mov ebx, [esp + 8]
    mov ecx, [esp + 12]
    mov edx, [esp + 16]

    imul ebx
    mov [r_imul1_va], eax
    mov [r_imul1_va + 4], edx

    mov eax, [esp + 4]
    imul eax, ecx
    mov [r_imul2_va], eax

    mov eax, [esp + 16]
    imul eax, eax, 0x1234567
    mov [r_imul3_va], eax

    mov eax, [esp + 4]
    mov ebx, [esp + 8]
    mul ebx
    mov [r_mul32_va], eax
    mov [r_mul32_va + 4], edx

    mov ecx, [esp + 12]
    or ecx, 1
    xor edx, edx
    mov eax, [esp + 4]
    div ecx
    mov [r_div32_va], eax
    mov [r_div32_va + 4], edx

    mov ecx, [esp + 12]
    and ecx, 0x7fffffff
    or ecx, 1
    mov eax, [esp + 4]
    cdq
    idiv ecx
    mov [r_idiv32_va], eax
    mov [r_idiv32_va + 4], edx

    mov ecx, [esp + 12]
    and ecx, 0xffff
    or ecx, 1
    xor edx, edx
    mov eax, [esp + 4]
    div cx
    movzx eax, ax
    movzx edx, dx
    mov [r_div16_va], eax
    mov [r_div16_va + 4], edx

    mov ecx, [esp + 12]
    and ecx, 0x7fff
    or ecx, 1
    mov eax, [esp + 4]
    cwd
    idiv cx
    movzx eax, ax
    movzx edx, dx
    mov [r_idiv16_va], eax
    mov [r_idiv16_va + 4], edx

    mov eax, [esp + 4]
    mov ebx, [esp + 8]
    mul bl
    movzx eax, ax
    mov [r_mul8_va], eax

    movzx eax, byte ptr [esp + 4]
    mov ebx, [esp + 8]
    and ebx, 0xff
    or ebx, 1
    div bl
    movzx eax, ax
    mov [r_div8_va], eax

    mov ebx, [esp + 8]
    and ebx, 0x7f
    or ebx, 1
    mov eax, [esp + 4]
    cbw
    idiv bl
    movzx eax, ax
    mov [r_idiv8_va], eax
    ret
.size mul_div, .-mul_div

.globl sign_extend
.type sign_extend, @function
sign_extend:
    mov eax, [esp + 4]
    cdq
    mov [r_cdq_va], eax
    mov [r_cdq_va + 4], edx

    mov eax, 0x12345678
    mov ax, word ptr [esp + 8]
    cwde
    mov [r_cwde_va], eax

    mov eax, 0x12345678
    mov al, byte ptr [esp + 12]
    cbw
    movzx eax, ax
    mov [r_cbw_va], eax

    mov eax, [esp + 16]
    mov edx, 0x11111111
    cwd
    movzx eax, ax
    movzx edx, dx
    mov [r_cwd_va], eax
    mov [r_cwd_va + 4], edx

    movsx eax, byte ptr [esp + 4]
    mov [r_movsx8_va], eax
    movsx ebx, word ptr [esp + 8]
    mov [r_movsx16_va], ebx
    movzx ecx, byte ptr [esp + 12]
    mov [r_movzx8_va], ecx
    movzx edx, word ptr [esp + 16]
    mov [r_movzx16_va], edx

    mov eax, [esp + 4]
    movsx ebx, al
    movzx ecx, bx
    mov edx, [esp + 8]
    movsx esi, dx
    movzx edi, si
    ret
.size sign_extend, .-sign_extend

.globl misc_control
.type misc_control, @function
misc_control:
    lea ebx, [xlat_table_va]
    mov eax, [esp + 4]
    and eax, 0x0f
    xlatb
    movzx edx, al
    mov [r_xlat_va], edx

    mov eax, [esp + 4]
    mov ecx, [esp + 8]
    cmp eax, ecx
    lahf
    movzx edx, ah
    mov [r_lahf_va], edx
    mov ah, 0xd5
    sahf
    pushfd
    pop edx
    and edx, 0x8d5
    mov [r_sahf_va], edx

    mov ecx, [esp + 12]
    and ecx, 7
    inc ecx
    xor eax, eax
misc_loop:
    add eax, ecx
    loop misc_loop
    mov [r_loop_va], eax

    mov ecx, [esp + 16]
    and ecx, 7
    inc ecx
    xor edx, edx
misc_loope:
    lea edx, [edx + ecx]
    loope misc_loope
    mov [r_loope_va], edx

    mov ecx, [esp + 20]
    and ecx, 7
    inc ecx
    xor esi, esi
    or ecx, ecx
misc_loopne:
    lea esi, [esi + ecx]
    loopne misc_loopne
    mov [r_loopne_va], esi

    mov ecx, [esp + 24]
    and ecx, 3
    jecxz misc_jecxz_zero
    mov eax, 0x11111111
    jmp misc_jecxz_done
misc_jecxz_zero:
    mov eax, 0x22222222
misc_jecxz_done:
    mov [r_jecxz_va], eax

    mov eax, [esp + 28]
    mov ebx, [esp + 32]
    mov ecx, [esp + 36]
    lea edx, [eax + ebx*8 + 0x7fffffff]
    mov [r_lea1_va], edx
    lea esi, [ebx*4 + ecx + 0x80000000]
    mov [r_lea2_va], esi
    lea edi, [eax + eax*2]
    mov [r_lea3_va], edi
    lea eax, [ecx*8 + 0x12345678]
    mov [r_lea4_va], eax

    mov eax, [esp + 4]
    mov ecx, [esp + 8]
    cmp eax, ecx
    seto byte ptr [setcc_buf_va]
    setno byte ptr [setcc_buf_va + 1]
    setb byte ptr [setcc_buf_va + 2]
    setae byte ptr [setcc_buf_va + 3]
    sete byte ptr [setcc_buf_va + 4]
    setne byte ptr [setcc_buf_va + 5]
    setbe byte ptr [setcc_buf_va + 6]
    seta byte ptr [setcc_buf_va + 7]
    sets byte ptr [setcc_buf_va + 8]
    setns byte ptr [setcc_buf_va + 9]
    setp byte ptr [setcc_buf_va + 10]
    setnp byte ptr [setcc_buf_va + 11]
    setl byte ptr [setcc_buf_va + 12]
    setge byte ptr [setcc_buf_va + 13]
    setle byte ptr [setcc_buf_va + 14]
    setg byte ptr [setcc_buf_va + 15]

    mov eax, [cmov_vals_va]
    mov ebx, [cmov_vals_va + 4]
    mov ecx, [cmov_vals_va + 8]
    mov edx, [cmov_vals_va + 12]
    mov esi, [cmov_vals_va + 16]
    mov edi, [cmov_vals_va + 20]

    cmovb eax, dword ptr [cmov_vals_va + 24]
    cmovae ebx, dword ptr [cmov_vals_va + 28]
    cmove ecx, dword ptr [cmov_vals_va + 32]
    cmovne edx, dword ptr [cmov_vals_va + 36]
    cmovl esi, dword ptr [cmov_vals_va + 40]
    cmovge edi, dword ptr [cmov_vals_va + 44]
    ret
.size misc_control, .-misc_control
