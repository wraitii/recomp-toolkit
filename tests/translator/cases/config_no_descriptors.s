# config_no_descriptors -- the same CF-producing branch with lazy descriptors off,
# and an alternate dispatch table configuration supplied by a case directive.

# translate: dispatch = { shift = 4, max_probe = 15 }
# env: RK_NO_DESCRIPTORS=1
# input: arg0=0 arg1=0
# input: arg0=0x80000000 arg1=0x7fffffff
# input: arg0=1 arg1=0xffffffff

.text
.globl config_no_descriptors
.type config_no_descriptors, @function
config_no_descriptors:
    mov eax, [esp + 4]
    mov ecx, [esp + 8]
    cmp eax, ecx
    jb cfg_less
    sub eax, ecx
    stc
    adc eax, 0
    ret
cfg_less:
    sub ecx, eax
    mov eax, ecx
    ret
.size config_no_descriptors, .-config_no_descriptors
