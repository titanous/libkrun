// Parked arm64 receiver. Linked flat binary injected into a Linux PE Image.
// The kernel reserves [0x70000000,0x70200000); host copies exclude it.
// R+0x2000 words: ready, release, cpu_resume PA, GIC list PA, count,
// progress (1 replay, 2 entering kernel, 3 failed register poll).
// Bytes48..67 publish the injected kernel GNU ID; word9 is its length20.
// R+0x10000/0x11000 page tables; stack top R+0x20000.
.text
.global _start
_start:
    msr daifset, #0xf
    adr x0, _start
    ldr x1, =0x70000000
    adr x2, blob_end
    sub x2, x2, x0
    mov x3, #0
1:  add x4, x0, x3
    dc civac, x4
    add x4, x1, x3
    dc civac, x4
    add x3, x3, #64
    cmp x3, #0x20000
    b.lo 1b
    dsb sy
    mrs x5, sctlr_el1
    bic x5, x5, #1
    bic x5, x5, #(1 << 2)
    msr sctlr_el1, x5
    isb
    mov x3, #0
2:  ldr x4, [x0, x3]
    str x4, [x1, x3]
    add x3, x3, #8
    cmp x3, x2
    b.lo 2b
    dsb sy
    ic iallu
    dsb sy
    isb
    add x5, x1, #(relocated - _start)
    br x5
relocated:
    mov x20, x1
    add x0, x20, #0x2000
    add x1, x20, #0x20000
3:  stp xzr, xzr, [x0], #16
    cmp x0, x1
    b.lo 3b
    dsb sy
    add x0, x20, #0x10000
    add x1, x20, #0x11000
    orr x2, x1, #3
    str x2, [x0]
    ldr x6, =0x0060000000000401
    ldr x7, =0x0000000000000705
    mov x3, #0
4:  lsl x4, x3, #30
    cmp x3, #1
    csel x5, x7, x6, eq
    orr x4, x4, x5
    str x4, [x1, x3, lsl #3]
    add x3, x3, #1
    cmp x3, #4
    b.lo 4b
    mov x3, #16
5:  lsl x4, x3, #30
    orr x4, x4, x7
    str x4, [x1, x3, lsl #3]
    add x3, x3, #1
    cmp x3, #32
    b.lo 5b
    dsb sy
    ldr x0, =0xff00
    msr mair_el1, x0
    ldr x0, =0x500903510
    msr tcr_el1, x0
    add x0, x20, #0x10000
    msr ttbr0_el1, x0
    isb
    tlbi vmalle1
    dsb sy
    isb
    mrs x0, sctlr_el1
    orr x0, x0, #1
    orr x0, x0, #(1 << 2)
    orr x0, x0, #(1 << 12)
    bic x0, x0, #(1 << 19)
    msr sctlr_el1, x0
    isb
    add x0, x20, #(vectors - _start)
    msr vbar_el1, x0
    add x0, x20, #0x20000
    mov sp, x0
    msr cntv_ctl_el0, xzr
    isb
    add x21, x20, #0x2000
    adr x1, kernel_identity
    ldp x2, x3, [x1]
    stp x2, x3, [x21, #48]
    ldr w2, [x1, #16]
    str w2, [x21, #64]
    mov x2, #20
    str x2, [x21, #72]
    ldr x0, =0x5944414552564352
    stlr x0, [x21]
6:  add x0, x21, #8
    ldar x0, [x0]
    cbnz x0, release
    yield
    b 6b
release:
    ldr x22, [x21, #16]
    ldr x23, [x21, #24]
    ldr x24, [x21, #32]
    cbz x24, failed
    cmp x24, #256
    b.hi failed
    mov x0, #1
    str x0, [x21, #40]
7:  ldp x0, x1, [x23], #16
    ubfx x3, x0, #0, #48
    tbnz x0, #63, 8f
    str w1, [x3]
    b 9f
8:  str x1, [x3]
9:  dsb sy
    tbz x0, #62, 11f
    mov x5, #0x100000
10: ldr w4, [x3]
    tbz w4, #31, 11f
    subs x5, x5, #1
    b.ne 10b
    b failed
11: tbz x0, #61, 13f
    mov x5, #0x100000
12: ldr w4, [x3]
    tbz w4, #2, 13f
    subs x5, x5, #1
    b.ne 12b
    b failed
13: subs x24, x24, #1
    b.ne 7b
    mov x0, #2
    str x0, [x21, #40]
    dsb sy
    ic iallu
    tlbi vmalle1
    dsb sy
    isb
    br x22
failed:
    add x21, x20, #0x2000
    mov x0, #3
    str x0, [x21, #40]
    dsb sy
14: wfe
    b 14b
.ltorg
.balign 8
kernel_identity:
    .ascii "NITRO_KERNEL_ID_20___"
.balign 2048
vectors:
.rept 16
.balign 128
    b failed
.endr
.balign 8
blob_end:
