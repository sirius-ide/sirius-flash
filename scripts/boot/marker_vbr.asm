; Marker volume boot record: proves the MBR's handover, not just its jump.
;
; The contract mbr.asm is written against, and every real VBR will assume:
;
;   CS:IP  = 0000:7C00
;   DS:SI -> the 16-byte partition entry that was booted
;   DL     = the BIOS drive number it was read from
;
; Each part is checked, in that order, and a *different* marker names the first
; one that fails. So "SIRIUS-VBR-OK" on screen means the MBR handed over
; correctly, and "SIRIUS-VBR-BAD:x" means it reached us and got the contract
; wrong — which a marker that only printed on arrival could not tell from
; success. The drive number is printed after OK so real hardware can be
; compared with qemu's 0x80.
;
; Layout is what the installer (boottest.py vbr) requires: a short jump at
; 0x00, the OEM name, zeros through 0x59 where it splices this volume's BPB,
; code from 0x5A, and 0x55AA at the end. Not the VBR this project will ship:
; it loads nothing.

bits 16
cpu 386
org 0x7c00

start:
    jmp short main                  ; EB 58 90: the canonical FAT32 jump
    nop
    db "SIRIUSVB"                   ; 0x03-0x0A, OEM name: kept by the installer
    times 0x5A - ($ - $$) db 0      ; 0x0B-0x59, BPB: spliced in from the volume

main:
    cli
    cld
    xor ax, ax                      ; a stack of our own, below the code. Not
    mov ss, ax                      ; part of the contract, so not trusted.
    mov sp, 0x7c00

    ; BL names the check in progress; DS, SI and DL are read before anything
    ; touches them.
    mov bl, 'C'
    call .here                      ; pushes IP, which equals the org-relative
.here:                              ; address only if CS is 0
    pop ax
    cmp ax, .here
    jne bad
    mov bl, 'S'
    cmp byte [si], 0x80             ; DS:SI -> the active entry
    jne bad
    mov bl, 'L'
    mov eax, [si + 8]               ; whose starting LBA is this volume's
    cmp eax, [cs:start + 0x1C]      ; hidden-sectors field
    jne bad
    mov bl, 'D'
    test dl, 0x80                   ; and the drive is a hard disk
    jz bad

    call vga
    mov si, msg_ok
    call puts
    mov al, dl
    call puthex
    jmp halt

bad:
    call vga
    mov si, msg_bad
    call puts
    mov al, bl
    call putc

halt:
    hlt                             ; interrupts are off, so this is the end
    jmp halt

; --- output ------------------------------------------------------------------
; Straight into the VGA text buffer, as marker.asm does: fewer moving parts
; than int 0x10, and it is exactly what the harness reads back.

vga:
    xor ax, ax
    mov ds, ax                      ; our strings live at org 0x7C00, segment 0
    mov ax, 0xb800
    mov es, ax
    xor di, di
    ret

puts:
    lodsb
    test al, al
    jz .done
    call putc
    jmp puts
.done:
    ret

putc:                               ; AL -> screen, bright white on black
    mov ah, 0x0f
    stosw
    ret

puthex:                             ; AL -> two hex digits
    push ax
    shr al, 4
    call .nibble
    pop ax
    and al, 0x0f
.nibble:
    cmp al, 10
    jb .digit
    add al, 'a' - '0' - 10
.digit:
    add al, '0'
    jmp putc                        ; putc's ret returns to our caller

msg_ok:  db "SIRIUS-VBR-OK dl=", 0
msg_bad: db "SIRIUS-VBR-BAD:", 0

times 510 - ($ - $$) db 0
dw 0xaa55
