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
; mbr.bin beside this file is the binary core will ship, and must be what this
; source assembles to (nasm's default optimisation; 2.16 and 3.x agree byte for
; byte). The selftest and CI both check that, so a change here is a change there.
;
; 386 instructions: reading a 32-bit LBA out of the partition entry needs 32-bit
; registers. Nothing that boots off USB predates a 386 by twenty years.
;
; Two symbols exist for the selftest only, and the shipped binary is assembled
; without either: SIRIUS_TEST_FORCE_CHS pretends the BIOS has no INT 13h
; extensions, SIRIUS_TEST_DL=n pretends it handed over drive n.

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
%ifdef SIRIUS_TEST_DL
    mov dl, SIRIUS_TEST_DL          ; test builds only
%endif
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
    cmp dword [si + 8], 0           ; a partition at LBA 0 is this sector: reading
    je .bad_table                   ; it and jumping to it would run us forever
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
; LBA first. If the BIOS has no packet interface, or has one and the read still
; fails (GRUB falls back here too; syslinux learnt to), fall back to CHS — but
; never to the CHS fields in the partition entry. Partitioners fill those from
; an assumed 255-head geometry, not from what this BIOS reports, and forced
; under SeaBIOS that version read a head the disk did not have. The geometry
; comes from the BIOS itself (AH=08h) and the LBA is converted against it, which
; is what syslinux's and GRUB's MBRs do. No report of a BIOS booting USB as a
; hard disk without extensions has been found newer than 2010, so this path may
; never run; it costs 80 bytes, and the alternative is a stick that only says
; "read error" on exactly the machine it was asked to help.
;
; DL is reloaded from [drive] before every call: AH=41h is known to clobber it
; on some firmware (GRUB names AST BIOS 1.04).

read:
    mov word [LOAD + 510], 0        ; forget the 0x55AA the BIOS loaded us with: a
                                    ; read that returns CF=0 having transferred
                                    ; nothing would otherwise verify stale bytes and
                                    ; run this sector again, silently and forever
    mov word [dap + 2], 1           ; EDD writes the count transferred back here —
                                    ; 0 after a failure — and the packet is reused
    mov dl, [drive]
    mov ah, 0x41                    ; INT 13h extensions installed?
    mov bx, 0x55aa
    int 0x13
%ifdef SIRIUS_TEST_FORCE_CHS
    stc                             ; test builds only: pretend they are not
%endif
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
    jnc verify

.chs:
    ; AH=08h returns CH = max cylinder bits 7:0, CL[7:6] = bits 9:8,
    ; CL[5:0] = sectors per track, DH = max head, DL = the drive *count*, and on
    ; floppies ES:DI -> a parameter table. DI is our partition entry: keep it.
    push di
    push es
    mov dl, [drive]
    mov ah, 0x08
    int 0x13
    pop es
    pop di
    jc .unread                      ; no such drive
    movzx ebx, cl
    and bl, 0x3f                    ; sectors per track
    jz .give_up                     ; some BIOSes return CF=0 and 0 here
    movzx esi, dh
    inc esi                         ; heads = max head + 1
    mov eax, [di + 8]               ; starting LBA
    xor edx, edx
    div ebx                         ; eax = track, edx = sector - 1
    mov cl, dl
    inc cl                          ; sectors are 1-based
    xor edx, edx
    div esi                         ; eax = cylinder, edx = head
    cmp eax, 1023                   ; CHS cannot name a higher cylinder; a wrapped
    ja .give_up                     ; one reads a real sector that is the wrong one
    mov dh, dl                      ; head
    mov ch, al                      ; cylinder bits 7:0
    shl ah, 6
    or cl, ah                       ; cylinder bits 9:8 -> CL[7:6]
    mov bx, LOAD                    ; ES:BX = buffer, ES is still 0
    mov dl, [drive]
    mov ax, 0x0201                  ; read 1 sector
    int 0x13
    jnc verify
    cmp byte [drive], 0x80          ; a hard disk that exists and will not read
    jae .give_up                    ; is a read error, not a wrong number

.unread:
    ; The drive the BIOS named does not exist, or is a floppy number that will
    ; not read. Some firmware does hand over the wrong one — 0x00 or 0x01 for a
    ; disk it booted as 0x80 (GRUB's workaround), 0x81 for the first disk
    ; (syslinux's "garbage in DL") — so try 0x80 once, the way the Windows 7
    ; MBR does after its reads fail. Never for a hard disk that merely failed a
    ; read: a genuine 0x81 with a bad sector must not be answered by booting
    ; whatever sits at the same LBA of 0x80.
    cmp byte [drive], 0x80
    je .give_up
    mov byte [drive], 0x80
    jmp read
.give_up:
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
    cld                             ; a BIOS may have returned with DF set
.print:
    lodsb
    test al, al
    jz .halt
    mov ah, 0x0e                    ; teletype, so it follows the BIOS's cursor
    mov bx, 0x0007
    int 0x10
    jmp .print
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
