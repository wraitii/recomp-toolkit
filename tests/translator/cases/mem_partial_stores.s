# mem_partial_stores -- overlapping 8/16/32-bit stores, stale bytes and
# unaligned dword loads spanning adjacent slots.

.section .data
partial:
    .space 32
results:
    .space 32

.set partial_va, DATA_BASE + (partial - data_start)
.set results_va, DATA_BASE + (results - data_start)

.text
.globl mem_partial_stores
.type mem_partial_stores, @function
mem_partial_stores:
    mov dword ptr [partial_va], 0x11223344
    mov dword ptr [partial_va + 4], 0x55667788
    mov byte ptr [partial_va + 2], 0xAA
    mov byte ptr [partial_va + 5], 0xBB
    mov eax, [partial_va]
    mov ecx, [partial_va + 1]
    mov edx, [partial_va + 3]
    mov word ptr [partial_va + 6], 0xCCDD
    mov ebx, [partial_va + 4]
    mov esi, [partial_va + 2]
    mov dword ptr [results_va], eax
    mov dword ptr [results_va + 4], ecx
    mov dword ptr [results_va + 8], edx
    mov dword ptr [results_va + 12], ebx
    mov dword ptr [results_va + 16], esi

    mov dword ptr [partial_va + 8], 0xDEADBEEF
    movzx edi, word ptr [partial_va + 8]
    mov word ptr [partial_va + 8], 0x1234
    mov ebp, [partial_va + 8]
    mov dword ptr [partial_va + 12], 0xCAFEBABE
    mov byte ptr [partial_va + 12], 0x11
    mov ecx, [partial_va + 12]
    mov dword ptr [results_va + 20], edi
    mov dword ptr [results_va + 24], ebp
    mov dword ptr [results_va + 28], ecx
    ret
.size mem_partial_stores, .-mem_partial_stores
