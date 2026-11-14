/* Ring-0 external kernel fixtures. Entry is long mode under the embedding API. */
.global _start
.section .text
.code64
_start:
    cli
#if defined(STOP_MISMATCH)
    mov $1, %al
    outb %al, $0xe8
    mov $0xfe, %al
    outb %al, $0x64
#elif defined(STOP_FAULT)
    lidt empty_idt(%rip)
    int3
#endif
1:  hlt
    jmp 1b
empty_idt:
    .word 0
    .quad 0
.section .note.GNU-stack,"",@progbits
