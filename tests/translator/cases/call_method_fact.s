# call_method_fact -- a vtable CALL with a COM method fact emits the guarded direct
# host call. The harness cannot run a host entry, so an initialized .data word that
# no input changes keeps the site unreached while the rest of the function compares.
# method: site=method_site identity=d3d9.dll!IDirect3DDevice9::SetRenderState argc=3

.section .data
armed:
    .long 0
object:
    .long vtable_va
vtable:
    .long 0, 0, 0
.set armed_va, DATA_BASE + (armed - data_start)
.set object_va, DATA_BASE + (object - data_start)
.set vtable_va, DATA_BASE + (vtable - data_start)

.text
.globl call_method_fact
.type call_method_fact, @function
call_method_fact:
    mov eax, [esp + 4]
    lea eax, [eax + eax * 2 + 7]
    cmp dword ptr [armed_va], 0x5a17c0de
    jne skip
    push ecx
    push eax
    push offset object_va
    mov edx, offset object_va
    mov edx, [edx]
method_site:
    call dword ptr [edx + 8]
skip:
    xor eax, [esp + 8]
    ret
.size call_method_fact, .-call_method_fact
