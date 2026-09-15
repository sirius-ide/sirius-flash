; Known-good MBR, used only to prove the harness can tell a booting disk from a
; dead one. It is NOT the MBR this project will ship: it ignores the partition
; table entirely and chainloads nothing.
;
; 440 bytes, not 446 or 512 — bytes 440-443 are the disk identifier and 446-509
; are the partition table, both of which the installer must leave alone.

bits 16
org 0x7c00

start:
    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7c00              ; stack below us, growing away from the code
    mov ax, 0xb800              ; write the VGA text buffer directly rather than
    mov es, ax                  ; via int 0x10: fewer moving parts to be wrong,
    xor di, di                  ; and it is exactly what the harness reads back
    mov si, msg
    mov ah, 0x0f                ; bright white on black
.next:
    lodsb
    test al, al
    jz .halt
    mov [es:di], al
    mov [es:di + 1], ah
    add di, 2
    jmp .next
.halt:
    hlt                         ; interrupts are off, so this is the end
    jmp .halt

msg: db "SIRIUS-MBR-OK", 0

times 440 - ($ - $$) db 0
