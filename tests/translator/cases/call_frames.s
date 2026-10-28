# call_frames -- callees writing their own argument slots and a thiscall stack object the
# caller rereads, a callee-saved register live across a call, EBP-chain reads of the
# caller's locals, and one helper reached from two stack depths.
# input: arg0=0 arg1=0
# input: arg0=0xffffffff arg1=1
# input: arg0=0x80000000 arg1=0x7fffffff

.section .data
frame_results:
    .space 32
.set frame_results_va, DATA_BASE + (frame_results - data_start)

.text
.globl call_frames
.type call_frames, @function
call_frames:
    push ebp
    mov ebp, esp
    push ebx
    push esi
    sub esp, 12
    mov ebx, [ebp + 8]
    mov esi, [ebp + 12]
    mov dword ptr [ebp - 12], ebx
    mov dword ptr [ebp - 16], esi
    mov dword ptr [ebp - 20], 0x5a5a5a5a

    push esi
    push ebx
    call scribble_args
    mov ecx, [esp]
    mov edx, [esp + 4]
    add esp, 8
    mov dword ptr [frame_results_va], ecx
    mov dword ptr [frame_results_va + 4], edx
    mov dword ptr [frame_results_va + 8], eax

    lea ecx, [ebp - 20]
    push esi
    call object_update
    mov eax, [ebp - 20]
    mov edx, [ebp - 16]
    mov dword ptr [frame_results_va + 12], eax
    mov dword ptr [frame_results_va + 16], edx

    call read_caller_frame
    mov dword ptr [frame_results_va + 20], eax

    push ebx
    call depth_helper
    add esp, 4
    mov ecx, eax
    sub esp, 12
    push esi
    call depth_helper
    add esp, 16
    add eax, ecx

    add eax, ebx
    xor eax, esi
    add esp, 12
    pop esi
    pop ebx
    pop ebp
    ret
.size call_frames, .-call_frames

.type scribble_args, @function
scribble_args:
    push ebx
    mov ebx, [esp + 8]
    mov eax, [esp + 12]
    not ebx
    mov dword ptr [esp + 8], ebx
    add dword ptr [esp + 12], 0x01000001
    lea eax, [eax + ebx]
    pop ebx
    ret
.size scribble_args, .-scribble_args

.type object_update, @function
object_update:
    mov eax, [esp + 4]
    xor dword ptr [ecx], eax
    mov edx, [ecx + 8]
    rol edx, 7
    mov dword ptr [ecx + 4], edx
    ret 4
.size object_update, .-object_update

.type read_caller_frame, @function
read_caller_frame:
    mov eax, [ebp - 12]
    sub eax, [ebp - 16]
    xor eax, [ebp - 20]
    ret
.size read_caller_frame, .-read_caller_frame

.type depth_helper, @function
depth_helper:
    mov eax, [esp + 4]
    lea edx, [esp + 4]
    and edx, 0xff
    imul eax, eax, 9
    add eax, edx
    ret
.size depth_helper, .-depth_helper
