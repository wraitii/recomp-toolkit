# x87_memory_widths -- fld/fst/fstp at 16/32/64/80 bits, stack spills,
# fild/fistp widths and frndint/fistp under all four control-word rounding modes.
# input: arg0=0x7fffffff arg1=0
# input: arg0=0x80000000 arg1=0xffffffff
# input: arg0=0xffffffff arg1=0xffffffff
# input: arg0=0x00000000 arg1=0x7fffffff

.section .data
f32_15:
    .float 1.5
t80_neg:
    .byte 0,0,0,0,0,0,0,0x80,0xfc,0xbf
i16_v:
    .word -1234
i32_v:
    .long 123456789
i64_v:
    .quad 0x0000000100000002
i64_neg:
    .quad -4886718345
round_cws:
    .word 0x037f, 0x077f, 0x0b7f, 0x0f7f
round_values:
    .double 2.5, 3.5, -2.5, -3.5
    .double 0.5, -0.5, 1.5, -1.5
out_q0: .zero 8
out_q1: .zero 8
out_q2: .zero 8
out_t0: .zero 10
out_d0: .zero 4
out_d1: .zero 4
out_di0: .zero 4
out_di1: .zero 4
out_qi0: .zero 8
out_qi1: .zero 8
out_qi2: .zero 8
out_w0: .zero 2
out_ia0: .zero 8
out_ia1: .zero 4
out_ia2: .zero 8
out_ia3: .zero 4
cw_saved: .zero 2
round_out: .zero 128
round_fist_out: .zero 128

.set f32_15_va, DATA_BASE + (f32_15 - data_start)
.set t80_neg_va, DATA_BASE + (t80_neg - data_start)
.set i16_v_va, DATA_BASE + (i16_v - data_start)
.set i32_v_va, DATA_BASE + (i32_v - data_start)
.set i64_v_va, DATA_BASE + (i64_v - data_start)
.set i64_neg_va, DATA_BASE + (i64_neg - data_start)
.set round_cws_va, DATA_BASE + (round_cws - data_start)
.set round_values_va, DATA_BASE + (round_values - data_start)
.set out_q0_va, DATA_BASE + (out_q0 - data_start)
.set out_q1_va, DATA_BASE + (out_q1 - data_start)
.set out_q2_va, DATA_BASE + (out_q2 - data_start)
.set out_t0_va, DATA_BASE + (out_t0 - data_start)
.set out_d0_va, DATA_BASE + (out_d0 - data_start)
.set out_d1_va, DATA_BASE + (out_d1 - data_start)
.set out_di0_va, DATA_BASE + (out_di0 - data_start)
.set out_di1_va, DATA_BASE + (out_di1 - data_start)
.set out_qi0_va, DATA_BASE + (out_qi0 - data_start)
.set out_qi1_va, DATA_BASE + (out_qi1 - data_start)
.set out_qi2_va, DATA_BASE + (out_qi2 - data_start)
.set out_w0_va, DATA_BASE + (out_w0 - data_start)
.set out_ia0_va, DATA_BASE + (out_ia0 - data_start)
.set out_ia1_va, DATA_BASE + (out_ia1 - data_start)
.set out_ia2_va, DATA_BASE + (out_ia2 - data_start)
.set out_ia3_va, DATA_BASE + (out_ia3 - data_start)
.set cw_saved_va, DATA_BASE + (cw_saved - data_start)
.set round_out_va, DATA_BASE + (round_out - data_start)
.set round_fist_out_va, DATA_BASE + (round_fist_out - data_start)

.text
.globl x87_memory_widths
.type x87_memory_widths, @function
x87_memory_widths:
    push ebp
    mov ebp, esp
    fld dword ptr [f32_15_va]
    fstp qword ptr [out_q0_va]
    fld qword ptr [out_q0_va]
    fstp tbyte ptr [out_t0_va]
    fld tbyte ptr [out_t0_va]
    fst dword ptr [out_d0_va]
    fstp dword ptr [out_d1_va]
    fld tbyte ptr [t80_neg_va]
    fstp qword ptr [out_q1_va]
    sub esp, 16
    fld tbyte ptr [t80_neg_va]
    fstp tbyte ptr [esp]
    fld tbyte ptr [esp]
    fstp qword ptr [out_q2_va]
    add esp, 16
    fild word ptr [i16_v_va]
    fistp dword ptr [out_di0_va]
    fild dword ptr [i32_v_va]
    fistp qword ptr [out_qi0_va]
    fild qword ptr [i64_v_va]
    fistp qword ptr [out_qi1_va]
    fild qword ptr [i64_neg_va]
    fistp qword ptr [out_qi2_va]
    fild dword ptr [i32_v_va]
    fistp dword ptr [out_di1_va]
    fild word ptr [i16_v_va]
    fistp word ptr [out_w0_va]
    pop ebp
    xor eax, eax
    ret
.size x87_memory_widths, .-x87_memory_widths

.globl x87_round_modes
.type x87_round_modes, @function
x87_round_modes:
    push ebp
    mov ebp, esp
    push esi
    push edi
    push ebx
    fnstcw word ptr [cw_saved_va]
    lea esi, [round_values_va]
    lea edi, [round_out_va]
    lea ebx, [round_cws_va]
    mov ecx, 4
mode_frndint:
    fldcw word ptr [ebx]
    push ecx
    mov ecx, 8
value_frndint:
    fld qword ptr [esi]
    frndint
    fistp dword ptr [edi]
    add esi, 8
    add edi, 4
    dec ecx
    jnz value_frndint
    add ebx, 2
    pop ecx
    dec ecx
    jnz mode_frndint
    lea esi, [round_values_va]
    lea edi, [round_fist_out_va]
    lea ebx, [round_cws_va]
    mov ecx, 4
mode_fist:
    fldcw word ptr [ebx]
    push ecx
    mov ecx, 8
value_fist:
    fld qword ptr [esi]
    fistp dword ptr [edi]
    add esi, 8
    add edi, 4
    dec ecx
    jnz value_fist
    add ebx, 2
    pop ecx
    dec ecx
    jnz mode_fist
    fldcw word ptr [cw_saved_va]
    pop ebx
    pop edi
    pop esi
    pop ebp
    xor eax, eax
    ret
.size x87_round_modes, .-x87_round_modes

.globl x87_integer_args
.type x87_integer_args, @function
x87_integer_args:
    push ebp
    mov ebp, esp
    fild dword ptr [ebp + 8]
    fistp qword ptr [out_ia0_va]
    fild dword ptr [ebp + 8]
    fistp dword ptr [out_ia1_va]
    fild qword ptr [ebp + 8]
    fistp qword ptr [out_ia2_va]
    fild word ptr [ebp + 8]
    fistp dword ptr [out_ia3_va]
    pop ebp
    xor eax, eax
    ret
.size x87_integer_args, .-x87_integer_args
