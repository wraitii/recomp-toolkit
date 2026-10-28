# insn_shifts -- shld/shrd and rol/ror/rcl/rcr/sar/shl count masking, zero-count
# flag preservation, carry rotate chains and 64-bit adc/sbb.
# input: arg0=0 arg1=0x12345678 arg2=0x9abcdef0 arg3=0xffffffff arg4=0
# input: arg0=1 arg1=0x80000000 arg2=0x7fffffff arg3=0 arg4=0xffffffff
# input: arg0=31 arg1=0xffff0000 arg2=0x0000ffff arg3=0x12345678 arg4=0x9abcdef0
# input: arg0=32 arg1=0x12345678 arg2=0x9abcdef0 arg3=1 arg4=0
# input: arg0=33 arg1=0x7fffffff arg2=0x80000000 arg3=0xffffffff arg4=7
# input: arg0=0xffffffff arg1=0x01020304 arg2=0x05060708 arg3=0 arg4=0xffffffff

.section .data
flag_buf:
    .zero 32
sum64:
    .zero 8
diff64:
    .zero 8

.set flag_buf_va, DATA_BASE + (flag_buf - data_start)
.set sum64_va, DATA_BASE + (sum64 - data_start)
.set diff64_va, DATA_BASE + (diff64 - data_start)

.text
.globl shift_counts
.type shift_counts, @function
shift_counts:
    mov ecx, [esp + 4]
    mov eax, [esp + 8]
    mov ebx, [esp + 12]
    mov edx, [esp + 16]
    mov esi, [esp + 20]

    cmp eax, ebx
    shld eax, ebx, cl
    pushfd
    pop dword ptr [flag_buf_va]
    test cl, 0x1f
    jz shift_counts_shld_zero
    and dword ptr [flag_buf_va], 0x0c5
    jmp shift_counts_shld_done
shift_counts_shld_zero:
    and dword ptr [flag_buf_va], 0x8d5
shift_counts_shld_done:

    cmp edx, esi
    shrd edx, esi, cl
    pushfd
    pop dword ptr [flag_buf_va + 4]
    test cl, 0x1f
    jz shift_counts_shrd_zero
    and dword ptr [flag_buf_va + 4], 0x0c5
    jmp shift_counts_shrd_done
shift_counts_shrd_zero:
    and dword ptr [flag_buf_va + 4], 0x8d5
shift_counts_shrd_done:
    ret
.size shift_counts, .-shift_counts

.globl rotate_counts
.type rotate_counts, @function
rotate_counts:
    mov ecx, [esp + 4]
    mov eax, [esp + 8]
    mov ebx, [esp + 12]
    mov edx, [esp + 16]
    mov esi, [esp + 20]

    cmp eax, ebx
    rol eax, cl
    pushfd
    pop dword ptr [flag_buf_va]
    test cl, 0x1f
    jz rotate_rol_zero
    and dword ptr [flag_buf_va], 0x0c5
    jmp rotate_rol_done
rotate_rol_zero:
    and dword ptr [flag_buf_va], 0x8d5
rotate_rol_done:

    cmp ebx, edx
    ror ebx, cl
    pushfd
    pop dword ptr [flag_buf_va + 4]
    test cl, 0x1f
    jz rotate_ror_zero
    and dword ptr [flag_buf_va + 4], 0x0c5
    jmp rotate_ror_done
rotate_ror_zero:
    and dword ptr [flag_buf_va + 4], 0x8d5
rotate_ror_done:

    cmp edx, esi
    stc
    rcl edx, cl
    pushfd
    pop dword ptr [flag_buf_va + 8]
    test cl, 0x1f
    jz rotate_rcl_zero
    and dword ptr [flag_buf_va + 8], 0x0c5
    jmp rotate_rcl_done
rotate_rcl_zero:
    and dword ptr [flag_buf_va + 8], 0x8d5
rotate_rcl_done:

    cmp esi, eax
    stc
    rcr esi, cl
    pushfd
    pop dword ptr [flag_buf_va + 12]
    test cl, 0x1f
    jz rotate_rcr_zero
    and dword ptr [flag_buf_va + 12], 0x0c5
    jmp rotate_rcr_done
rotate_rcr_zero:
    and dword ptr [flag_buf_va + 12], 0x8d5
rotate_rcr_done:

    cmp eax, ebx
    sar eax, cl
    pushfd
    pop dword ptr [flag_buf_va + 16]
    test cl, 0x1f
    jz rotate_sar_zero
    and dword ptr [flag_buf_va + 16], 0x0c5
    jmp rotate_sar_done
rotate_sar_zero:
    and dword ptr [flag_buf_va + 16], 0x8d5
rotate_sar_done:

    cmp esi, edx
    shl esi, cl
    pushfd
    pop dword ptr [flag_buf_va + 20]
    test cl, 0x1f
    jz rotate_shl_zero
    and dword ptr [flag_buf_va + 20], 0x0c5
    jmp rotate_shl_done
rotate_shl_zero:
    and dword ptr [flag_buf_va + 20], 0x8d5
rotate_shl_done:
    ret
.size rotate_counts, .-rotate_counts

.globl adc_sbb64
.type adc_sbb64, @function
adc_sbb64:
    mov eax, [esp + 4]
    mov edx, [esp + 8]
    mov ebx, [esp + 12]
    mov ecx, [esp + 16]

    add eax, ebx
    adc edx, ecx
    mov [sum64_va], eax
    mov [sum64_va + 4], edx

    mov eax, [esp + 4]
    mov edx, [esp + 8]
    sub eax, ebx
    sbb edx, ecx
    mov [diff64_va], eax
    mov [diff64_va + 4], edx
    ret
.size adc_sbb64, .-adc_sbb64
