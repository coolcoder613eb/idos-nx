; ticktest.com — Test timer interrupt delivery
; Installs an INT 1Ch handler that counts ticks.
; Main loop prints "TICK" every ~1 second (~18 ticks).
;
; Assemble: nasm -f bin -o ticktest.com ticktest.asm

org 0x100

start:
    ; Save old INT 1Ch vector
    mov ax, 0x351C
    int 0x21
    mov [old_1c_off], bx
    mov [old_1c_seg], es

    ; Install our INT 1Ch handler
    mov ax, 0x251C
    mov dx, timer_handler
    int 0x21

    ; Print startup message
    mov ah, 0x09
    mov dx, msg_start
    int 0x21

    ; Ensure interrupts are enabled
    sti

    ; Main loop: wait for tick_flag to be set by the ISR
main_loop:
    cmp byte [tick_flag], 0
    je main_loop

    ; Clear the flag
    mov byte [tick_flag], 0

    ; Print "TICK"
    mov ah, 0x09
    mov dx, msg_tick
    int 0x21

    ; Increment and check count — exit after 10 ticks
    inc byte [tick_count]
    cmp byte [tick_count], 10
    jb main_loop

    ; Restore old INT 1Ch vector
    push ds
    mov dx, [old_1c_off]
    mov ax, [old_1c_seg]
    mov ds, ax
    mov ax, 0x251C
    int 0x21
    pop ds

    ; Print done message
    mov ah, 0x09
    mov dx, msg_done
    int 0x21

    ; Exit
    mov ax, 0x4C00
    int 0x21

; --- INT 1Ch handler ---
; Called ~18.2 times per second by the timer
timer_handler:
    inc word [cs:isr_counter]
    cmp word [cs:isr_counter], 18
    jb .done
    mov word [cs:isr_counter], 0
    mov byte [cs:tick_flag], 1
.done:
    ; Chain to old handler
    jmp far [cs:old_1c_off]

; --- Data ---
isr_counter  dw 0
tick_flag    db 0
tick_count   db 0
old_1c_off   dw 0
old_1c_seg   dw 0

msg_start    db 'Timer test: printing TICK every ~1 second (10 times)', 0x0D, 0x0A, '$'
msg_tick     db 'TICK', 0x0D, 0x0A, '$'
msg_done     db 'Done!', 0x0D, 0x0A, '$'
