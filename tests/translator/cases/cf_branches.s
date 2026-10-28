# cf_branches -- every Jcc condition on signed/unsigned edge inputs, plus a
# diamond merge of register and memory values from both arms.

.section .data
cf_jcc_result:
    .long 0
cf_diamond_mem:
    .long 0

.set cf_jcc_result_va, DATA_BASE + (cf_jcc_result - data_start)
.set cf_diamond_mem_va, DATA_BASE + (cf_diamond_mem - data_start)

# input: arg0=0 arg1=0
# input: arg0=0xffffffff arg1=0xffffffff
# input: arg0=0x80000000 arg1=0
# input: arg0=0 arg1=0x80000000
# input: arg0=0x80000000 arg1=0x7fffffff
# input: arg0=7 arg1=11

.text
.globl cf_jcc_all
.type cf_jcc_all, @function
cf_jcc_all:
    mov eax, [esp + 4]
    mov ebx, [esp + 8]
    xor ecx, ecx
    cmp eax, ebx
    jo cf_jcc_1
    or ecx, 0x0001
cf_jcc_1:
    cmp eax, ebx
    jno cf_jcc_2
    or ecx, 0x0002
cf_jcc_2:
    cmp eax, ebx
    jb cf_jcc_3
    or ecx, 0x0004
cf_jcc_3:
    cmp eax, ebx
    jae cf_jcc_4
    or ecx, 0x0008
cf_jcc_4:
    cmp eax, ebx
    je cf_jcc_5
    or ecx, 0x0010
cf_jcc_5:
    cmp eax, ebx
    jne cf_jcc_6
    or ecx, 0x0020
cf_jcc_6:
    cmp eax, ebx
    jbe cf_jcc_7
    or ecx, 0x0040
cf_jcc_7:
    cmp eax, ebx
    ja cf_jcc_8
    or ecx, 0x0080
cf_jcc_8:
    cmp eax, ebx
    js cf_jcc_9
    or ecx, 0x0100
cf_jcc_9:
    cmp eax, ebx
    jns cf_jcc_10
    or ecx, 0x0200
cf_jcc_10:
    cmp eax, ebx
    jl cf_jcc_11
    or ecx, 0x0400
cf_jcc_11:
    cmp eax, ebx
    jge cf_jcc_12
    or ecx, 0x0800
cf_jcc_12:
    cmp eax, ebx
    jle cf_jcc_13
    or ecx, 0x1000
cf_jcc_13:
    cmp eax, ebx
    jg cf_jcc_14
    or ecx, 0x2000
cf_jcc_14:
    mov dword ptr [cf_jcc_result_va], ecx
    mov eax, ecx
    ret
.size cf_jcc_all, .-cf_jcc_all

.globl cf_diamond
.type cf_diamond, @function
cf_diamond:
    mov eax, [esp + 4]
    mov ebx, [esp + 8]
    cmp eax, ebx
    jle cf_dia_less_equal
    mov ecx, eax
    sub ecx, ebx
    mov edx, 0x1111
    mov dword ptr [cf_diamond_mem_va], ecx
    jmp cf_dia_merge
cf_dia_less_equal:
    mov ecx, ebx
    sub ecx, eax
    mov edx, 0x2222
    mov dword ptr [cf_diamond_mem_va], ecx
cf_dia_merge:
    mov eax, [cf_diamond_mem_va]
    add eax, ecx
    xor eax, edx
    mov dword ptr [cf_diamond_mem_va], eax
    ret
.size cf_diamond, .-cf_diamond
