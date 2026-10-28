# cf_jump_table -- bounds-checked switches through .data jump tables: byte-indexed
# two-level table, duplicate targets, default case, and direct table dispatch.

.section .data
cf_two_level_map:
    .byte 0, 1, 2, 3, 1, 2, 0, 3
cf_two_level_table:
    .long cf_jt_case0_va
    .long cf_jt_case1_va
    .long cf_jt_case2_va
    .long cf_jt_case3_va
cf_direct_table:
    .long cf_dup_case0_va
    .long cf_dup_case0_va
    .long cf_dup_case1_va
    .long cf_dup_case2_va
    .long cf_dup_case0_va
    .long cf_dup_case2_va

.set cf_two_level_map_va, DATA_BASE + (cf_two_level_map - data_start)
.set cf_two_level_table_va, DATA_BASE + (cf_two_level_table - data_start)
.set cf_direct_table_va, DATA_BASE + (cf_direct_table - data_start)

.set cf_jt_case0_va, TEXT_BASE + (cf_jt_case0 - case_start)
.set cf_jt_case1_va, TEXT_BASE + (cf_jt_case1 - case_start)
.set cf_jt_case2_va, TEXT_BASE + (cf_jt_case2 - case_start)
.set cf_jt_case3_va, TEXT_BASE + (cf_jt_case3 - case_start)
.set cf_dup_case0_va, TEXT_BASE + (cf_dup_case0 - case_start)
.set cf_dup_case1_va, TEXT_BASE + (cf_dup_case1 - case_start)
.set cf_dup_case2_va, TEXT_BASE + (cf_dup_case2 - case_start)

# table: site=cf_jt_site targets=cf_jt_case0,cf_jt_case1,cf_jt_case2,cf_jt_case3
# table: site=cf_dup_site targets=cf_dup_case0,cf_dup_case0,cf_dup_case1,cf_dup_case2,cf_dup_case0,cf_dup_case2
# input: arg0=0
# input: arg0=1
# input: arg0=2
# input: arg0=3
# input: arg0=4
# input: arg0=5
# input: arg0=6
# input: arg0=7
# input: arg0=8
# input: arg0=0xffffffff

.text
.globl cf_jt_site
.globl cf_dup_site
.globl cf_jump_table
.type cf_jump_table, @function
cf_jump_table:
    mov eax, [esp + 4]
    cmp eax, 7
    ja cf_jt_default
    movzx eax, byte ptr [cf_two_level_map_va + eax]
    mov edx, 0x100
    lea ebx, [cf_two_level_table_va]
cf_jt_site:
    jmp dword ptr [ebx + eax*4]
cf_jt_case0:
    add edx, 1
    jmp cf_jt_out
cf_jt_case1:
    add edx, 2
    jmp cf_jt_out
cf_jt_case2:
    add edx, 4
    jmp cf_jt_out
cf_jt_case3:
    add edx, 8
    jmp cf_jt_out
cf_jt_default:
    mov edx, 0xdead
cf_jt_out:
    mov eax, edx
    ret
.size cf_jump_table, .-cf_jump_table

.globl cf_switch_dup
.type cf_switch_dup, @function
cf_switch_dup:
    mov eax, [esp + 4]
    cmp eax, 5
    ja cf_dup_default
cf_dup_site:
    jmp dword ptr [cf_direct_table_va + eax*4]
cf_dup_case0:
    mov eax, 0x10
    rep ret
cf_dup_case1:
    mov eax, 0x20
    ret
cf_dup_case2:
    mov eax, 0x30
    rep ret
cf_dup_default:
    mov eax, -1
    ret
.size cf_switch_dup, .-cf_switch_dup
