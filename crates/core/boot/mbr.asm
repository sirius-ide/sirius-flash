; Sirius Flash MBR — chainload the active partition.
;
; Written here rather than lifted. Rufus carries src/ms-sys/, whose *logic* is
; GPL-2.0-or-later and usable, but whose blobs are a different question:
; br_fat32pe_0x52.h is Microsoft's boot record (it contains "BOOTMGR is
; missing"), and the GPL header covers Henrik Carlqvist's code, not Microsoft's
; bytes. Their MBR (mbr_rufus.h) is pbatard's own and clean, but the boot record
; beside it is not, and both need per-volume BPB patching anyway — so there was
; never a version of this where we copy bytes and skip understanding them.
;
; The BIOS loads us at 0x7C00 with DL set to the drive we came from, and the
; volume boot record we are about to load expects to run at that same address.
; So the first thing we do is move out of the way.
;
; Assembled to exactly 440 bytes: 440-443 is the disk identifier and 446-509 the
; partition table, and an MBR that overwrites either is not an MBR any more.
;
; 386 instructions: reading a 32-bit LBA out of the partition entry needs 32-bit
; registers. Nothing that boots off USB predates a 386 by twenty years.

bits 16
cpu 386
org 0x600

RELOC   equ 0x0600                  ; where we move ourselves
LOAD    equ 0x7c00                  ; where the BIOS put us, and where the VBR goes
PTABLE  equ RELOC + 446

; ---------------------------------------------------------------------------

start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, LOAD                    ; stack grows down from the load address,
    cld                             ; so it can never reach the relocated code
    mov si, LOAD
    mov di, RELOC
    mov cx, 256
    rep movsw
    jmp 0:main                      ; far jump: also normalises CS to 0

main:
    sti
    mov [drive], dl                 ; the VBR needs this too; keep it somewhere safe

; --- find the one active partition ------------------------------------------
; Exactly one. Zero is a disk nobody marked bootable; two is a disk where the
; choice would be arbitrary, and picking one silently is how you boot the wrong
; installer. The status byte is 0x00 or 0x80 and nothing else — anything else
; means we are not looking at a partition table at all, and reading a sector
; based on it would be a guess.

    xor di, di                      ; di = the active entry, 0 = none seen yet
    mov si, PTABLE
    mov cx, 4
.scan:
    mov al, [si]
    cmp al, 0x80
    jne .inactive
    test di, di
    jnz .bad_table
    mov di, si
    jmp .next
.inactive:
    test al, al
    jnz .bad_table
.next:
    add si, 16
    loop .scan
    test di, di
    jnz read
    mov si, msg_noactive
    jmp fail
.bad_table:
    mov si, msg_badtable
    jmp fail

; --- read its first sector to 0x7C00 ----------------------------------------
; LBA first. The CHS fields in a partition entry cap out at 1024 cylinders and
; are filled in for compatibility rather than for use; on anything made this
; century they describe a geometry the drive does not have. CHS stays as a
; fallback because a BIOS that cannot do packet access cannot be told to.

read:
    mov dl, [drive]
    mov ah, 0x41                    ; INT 13h extensions installed?
    mov bx, 0x55aa
    int 0x13
    jc .chs
    cmp bx, 0xaa55
    jne .chs
    test cl, 1                      ; bit 0: packet access supported
    jz .chs

    mov eax, [di + 8]               ; starting LBA, from the partition entry
    mov [dap_lba], eax
    mov si, dap
    mov dl, [drive]
    mov ah, 0x42
    int 0x13
    jnc verify                      ; on failure fall through and try CHS: a
                                    ; drive that advertises packets and then
                                    ; refuses one has nothing else to offer, but
                                    ; trying costs two bytes
.chs:
    mov dh, [di + 1]                ; head
    mov cx, [di + 2]                ; sector in CL (with cylinder high bits), cylinder low in CH
    mov bx, LOAD                    ; ES:BX = buffer, ES is still 0
    mov dl, [drive]
    mov ax, 0x0201                  ; read 1 sector
    int 0x13
    jnc verify
    mov si, msg_read
    jmp fail

; --- hand over ---------------------------------------------------------------

verify:
    cmp word [LOAD + 510], 0xaa55
    je handover
    mov si, msg_nosig               ; a partition marked active whose first
    jmp fail                        ; sector is not a boot sector

handover:
    mov si, di                      ; DS:SI -> the partition entry we booted,
    mov dl, [drive]                 ; DL -> the drive: the contract every
    jmp 0:LOAD                      ; volume boot record is written against

; --- failure -----------------------------------------------------------------
; Say something. Media that halts in silence is indistinguishable from a dead
; drive, a bad USB port, or firmware that never tried — and the user's next move
; depends on which it was.

fail:
    lodsb
    test al, al
    jz .halt
    mov ah, 0x0e                    ; teletype, so it follows the BIOS's cursor
    mov bx, 0x0007
    int 0x10
    jmp fail
.halt:
    cli
    hlt
    jmp .halt

; ---------------------------------------------------------------------------

drive:          db 0

dap:            db 0x10             ; packet size
                db 0                ; reserved
                dw 1                ; sectors to read
                dw LOAD             ; offset
                dw 0                ; segment
dap_lba:        dd 0                ; LBA, low
                dd 0                ; LBA, high

msg_noactive:   db "Sirius: no active partition", 13, 10, 0
msg_badtable:   db "Sirius: bad partition table", 13, 10, 0
msg_read:       db "Sirius: read error", 13, 10, 0
msg_nosig:      db "Sirius: not a boot sector", 13, 10, 0

times 440 - ($ - $$) db 0
