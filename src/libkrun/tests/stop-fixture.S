/* Ring-0 external kernel fixtures: long-mode entry supplied by the VMM. */
.global _start
.section .text
.code64
_start:
    cli
#if defined(STOP_REASON)
    mov $STOP_REASON, %al
    outb %al, $0xea
#endif
#if defined(STOP_CLEAR)
    xor %al, %al
    outb %al, $0xea
#endif
#if defined(STOP_RESET)
    mov $0xfe, %al
    outb %al, $0x64
#endif
    lidt empty_idt(%rip)
    int3
1:  hlt
    jmp 1b
empty_idt:
    .word 0
    .quad 0
.section .note.GNU-stack,"",@progbits
