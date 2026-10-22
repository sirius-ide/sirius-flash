; Stand-in BOOTMGR: proves the FAT32 VBR loaded all of it, in order, and
; entered it the way the real one is entered.
;
; The real BOOTMGR (Windows 11, 473,364 bytes) was loaded under qemu to learn
; what it needs: the whole file at 2000:0000, entered there by a far jump; the
; boot sector still at 0:7C00 with bytes-per-sector and hidden sectors intact;
; and its drive number in BPB byte 0x40, which it reads instead of DL. So this
; checks exactly those, in order, and prints SIRIUS-BOOTMGR-BAD:x naming the
; first that fails, or SIRIUS-BOOTMGR-OK dl=NN.
;
; The file is this code in its first sector, then sector k (k = 1..) beginning
; with the dword k, and the four bytes "END!" at the very end — the selftest
; builds that payload around this binary. A loader that loaded too little, out
; of order, or the wrong clusters cannot pass the sector check; one that
; stopped a sector short cannot pass the tail check; one that loaded whole
; clusters, or a sector too many, cannot pass the over-read check, because the
; selftest pads the rest of the last cluster with 0xA5 and the memory past the
; file starts out zero. SECTORS, HIDDEN and SIZE are assembled in (-D) by the
; selftest, which knows all three.

bits 16
cpu 386
org 0

%ifndef SECTORS
%error "assemble with -DSECTORS=<full sectors after this one> -DHIDDEN=<partition start LBA> -DSIZE=<file size>"
%endif

start:
    jmp short main
    nop

main:
    cli
    mov bl, dl                      ; DL as given, kept for the report
    mov ax, cs
    mov dx, ax                      ; CS as given
    xor ax, ax
    mov ss, ax
    mov sp, 0x7000
    mov ds, ax                      ; everything below reads segment 0

    mov cl, 'C'                     ; entered at 2000:0000?
    cmp dx, 0x2000
    jne bad
    call .here
.here:
    pop ax
    cmp ax, .here
    jne bad
    mov cl, 'D'                     ; DL is a hard disk, and the BPB agrees
    test bl, 0x80
    jz bad
    cmp [0x7c40], bl
    jne bad
    mov cl, 'B'                     ; bytes per sector still 512
    cmp word [0x7c0b], 512
    jne bad
    mov cl, 'H'                     ; hidden sectors still the partition start
    cmp dword [0x7c1c], HIDDEN
    jne bad
    mov cl, 'S'                     ; sector k starts with dword k
    mov esi, 1
.sector:
    mov ax, si
    shl ax, 5
    add ax, 0x2000
    mov es, ax
    cmp [es:0], esi
    jne bad
    inc esi
    cmp esi, SECTORS
    jbe .sector
    mov cl, 'T'                     ; and the tail is all there
    mov ax, 0x2000 + ((SIZE - 4) >> 4)
    mov es, ax
    cmp dword [es:(SIZE - 4) & 15], "END!"
    jne bad
    mov cl, 'X'                     ; and nothing past the file was loaded: the
    mov ax, 0x2000 + ((SIZE + 511) >> 9) * 32
    mov es, ax                      ; selftest pads the last cluster with 0xA5,
    cmp dword [es:0], 0             ; and fresh memory is zero
    jne bad

    call vga
    push cs
    pop ds
    mov si, msg_ok
    call puts
    mov al, bl
    call puthex
    jmp halt

bad:
    mov edx, esi                    ; the sector in question: puts clobbers SI
    call vga
    push cs
    pop ds
    mov si, msg_bad
    call puts
    mov al, cl
    call putc
    cmp cl, 'S'
    jne halt
    mov al, ':'
    call putc
    mov eax, edx
    call puthex32

halt:
    hlt                             ; interrupts are off, so this is the end
    jmp halt

; --- output: straight into the VGA text buffer, as the other markers do -------

vga:
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

puthex32:                           ; EAX -> eight hex digits
    push eax
    shr eax, 16
    call puthex16
    pop eax
puthex16:                           ; AX -> four
    push ax
    mov al, ah
    call puthex
    pop ax
puthex:                             ; AL -> two
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

msg_ok:  db "SIRIUS-BOOTMGR-OK dl=", 0
msg_bad: db "SIRIUS-BOOTMGR-BAD:", 0

%if ($ - $$) > 512
%error "the stand-in must fit in the first sector; the rest of the file is numbered"
%endif
times 512 - ($ - $$) db 0
