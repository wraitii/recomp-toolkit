# insn_bitops -- bt/bts/btr/btc register and memory bit offsets, bswap/bsr,
# xadd, high-byte partial writes, 16-bit arithmetic and inc/dec preserving CF.
# input: arg0=0 arg1=0 arg2=0 arg3=0 arg4=0
# input: arg0=31 arg1=0x7f arg2=0x80000000 arg3=0xffffffff arg4=0xffffffff
# input: arg0=32 arg1=0x20 arg2=1 arg3=1 arg4=0
# input: arg0=0xffffffff arg1=0x7f arg2=0x12345678 arg3=0x100 arg4=0x80000000

.section .data
bit_table:
    .long 0x12345678, 0x9abcdef0, 0x0f0f0f0f, 0xffff0000

.set bit_table_va, DATA_BASE + (bit_table - data_start)

.text
.globl bitops
.type bitops, @function
bitops:
    mov ecx, [esp + 4]
    mov edx, [esp + 8]
    mov eax, [bit_table_va]
    bt eax, ecx
    xor esi, esi
    adc esi, 0

    and edx, 0x7f
    bts dword ptr [bit_table_va], edx
    adc esi, 0
    btr dword ptr [bit_table_va], edx
    adc esi, 0
    btc dword ptr [bit_table_va], edx
    adc esi, 0
    bt dword ptr [bit_table_va], edx
    adc esi, 0

    mov eax, [bit_table_va + 4]
    bswap eax
    mov edi, [bit_table_va + 8]
    bswap edi

    mov ebx, [esp + 12]
    test ebx, ebx
    jz bitops_no_bsr_a
    bsr ebp, ebx
bitops_no_bsr_a:
    mov ecx, [esp + 16]
    test ecx, ecx
    jz bitops_no_bsr_b
    bsr ebx, ecx
bitops_no_bsr_b:
    mov edx, [esp + 20]
    xadd eax, edx
    ret
.size bitops, .-bitops

.globl partial_writes
.type partial_writes, @function
partial_writes:
    mov eax, [esp + 4]
    mov ebx, [esp + 8]
    mov ecx, [esp + 12]
    mov edx, [esp + 16]

    mov ah, bl
    add al, bh
    mov ch, al
    movzx esi, ch

    add ax, cx
    adc dx, cx
    sub bx, ax

    stc
    inc eax
    adc esi, 0
    clc
    dec ebx
    adc esi, 0

    neg ecx
    not edx
    ret
.size partial_writes, .-partial_writes
