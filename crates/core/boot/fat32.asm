; Sirius Flash FAT32 volume boot record — load BOOTMGR from the root directory.
;
; One sector. Microsoft's FAT32 boot code spans sector 0 and reserved sector 12
; (plus a CHS fallback and active-FAT selection we do not carry); FreeDOS and
; KolibriOS load a kernel from a FAT32 root directory in a single sector, and
; so does this. It walks the filesystem honestly — the whole root directory
; chain, the whole file chain, clusters of any size — and assumes nothing about
; where BOOTMGR's clusters are.
;
; What BOOTMGR needs on entry, established by loading the real one (Windows 11,
; 473,364 bytes) under qemu and taking things away until it broke:
;
;   * the WHOLE file at linear 0x20000, entered by a far jump to 2000:0000 —
;     loading all but the last cluster gets "BOOTMGR image is corrupt";
;   * this boot sector still at 0:7C00, with bytes-per-sector (0x0B) and hidden
;     sectors (0x1C) intact — it reads both from memory;
;   * its BIOS drive number in BPB byte 0x40 (BS_DrvNum): it ignores DL and
;     reads that field, which is why Rufus patches 0x80 into it at install time.
;     We store the DL we were given there, so a stick booted as 0x81 works too.
;
; Everything else about the entry state — DS, ES, SS:SP, SI, BP — it sets up
; for itself and ignores.
;
; Layout is what the installer requires: a short jump at 0x00, the OEM name,
; zeros through 0x59 where it splices this volume's BPB, code from 0x5A, and
; 0x55AA at the end. It fits only at nasm's default optimisation (-Ox; -O0 is
; 18 bytes over, and says so). The installer also enforces what this code assumes:
; 512-byte sectors (the FAT-entries-per-sector shift and the 32-paragraph
; segment step both say so), and hidden sectors equal to the partition's
; start. The MBR's DS:SI is deliberately not consulted for that: a foreign MBR
; may pass anything there, and a wrong "correction" is worse than none.
;
; Memory: the directory sector is read to 0:7E00 and the FAT sector to 0:8000,
; both above this code; the stack grows down from 7C00. The file is loaded by
; its directory-entry size, one sector at a time, so clusters larger than the
; file's tail do not drag the load past the file — with 64 KiB clusters the
; last whole cluster of a 473 KB BOOTMGR would otherwise end at 0xA0000, on
; top of the EBDA. The load also stops at linear 0x9F000 whatever the size
; field says: past there lie the EBDA, video memory and, after the segment
; wraps, the interrupt table and this sector, and a file that large is not
; BOOTMGR (Windows 11's is 473 KB, ending at 0x93A00). BOOTMGR then reports
; its own image corrupt, which is louder than a machine scribbled over.

bits 16
cpu 386
org 0x7c00

LOADSEG equ 0x2000                  ; BOOTMGR goes here (linear 0x20000)
DIRBUF  equ 0x7e00                  ; one root-directory sector
FATBUF  equ 0x8000                  ; one FAT sector

start:
    jmp short main                  ; EB 58 90: the canonical FAT32 jump
    nop
    db "SIRIUSVB"                   ; 0x03-0x0A, OEM name: kept by the installer
    times 0x5A - ($ - $$) db 0      ; 0x0B-0x59, BPB: spliced in from the volume

main:
    cli
    cld                             ; the name compare and the messages depend on it
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov bp, 0x7c00                  ; BPB fields are [bp + offset]
    mov sp, bp
    sti
    mov [bp + 0x40], dl             ; BS_DrvNum := the drive the BIOS booted

    ; first data sector = hidden + reserved + FATs * sectors-per-FAT
    movzx eax, byte [bp + 0x10]
    mul dword [bp + 0x24]
    movzx ecx, word [bp + 0x0e]
    add eax, ecx
    add eax, [bp + 0x1c]
    push eax                        ; [bp - 4]: first data sector
    push dword 0                    ; [bp - 8]: FAT sector held in FATBUF (none)

; --- find BOOTMGR in the root directory --------------------------------------
; Every cluster of the root chain, every sector of each, every 32-byte entry:
; the name must match and the entry must be a file — a volume label or a
; directory called BOOTMGR is not it, and long-name entries carry 0x0F, which
; the same test skips. The first 0x00 entry is the end of the directory.

    mov eax, [bp + 0x2c]            ; root directory's first cluster
.dir_cluster:
    cmp eax, 0x0ffffff8
    jae not_found
    push eax                        ; the cluster, for the FAT lookup after
    call cluster_lba
    movzx dx, byte [bp + 0x0d]      ; sectors left in this cluster
.dir_sector:
    mov bx, DIRBUF
    call read
    mov di, DIRBUF
.entry:
    cmp byte [di], 0
    je not_found
    test byte [di + 11], 0x18       ; directory, label, or long-name part
    jnz .next_entry
    mov si, name
    mov cx, 11
    push di
    repe cmpsb
    pop di
    je found
.next_entry:
    add di, 32
    cmp di, DIRBUF + 512
    jb .entry
    dec dx
    jnz .dir_sector
    pop eax
    call fat_next
    jmp .dir_cluster

; --- load it -----------------------------------------------------------------
; Clusters in chain order, sectors in cluster order, to LOADSEG:0 upward, the
; segment advancing 32 paragraphs per sector so no read crosses a 64 KiB line.
; The count comes from the entry's size; a chain that ends first leaves the
; rest unloaded and BOOTMGR to say "image is corrupt", which is the truth. The
; first cluster is read before its chain is consulted, so an entry whose first
; cluster is an end-of-chain mark cannot jump with nothing loaded.

found:
    pop eax                         ; the directory cluster, no longer needed
    mov ecx, [di + 0x1c]            ; file size in bytes
    add ecx, 511
    shr ecx, 9                      ; -> sectors
    jz not_found                    ; an empty BOOTMGR is no BOOTMGR
    mov ax, [di + 0x14]
    shl eax, 16
    mov ax, [di + 0x1a]             ; first cluster, high:low
    cmp eax, 2
    jb not_found
    mov bx, LOADSEG
    mov es, bx
.file_cluster:
    push eax
    call cluster_lba
    movzx dx, byte [bp + 0x0d]
.file_sector:
    xor bx, bx
    call read
    mov bx, es
    add bx, 512 / 16
    mov es, bx
    cmp bh, 0x9f                    ; linear 0x9F000: see the header
    jae go
    dec ecx
    jz go
    dec dx
    jnz .file_sector
    pop eax
    call fat_next
    cmp eax, 0x0ffffff8
    jb .file_cluster

go:
    mov dl, [bp + 0x40]
    push ds                         ; ES := 0 (DS already is)
    pop es
    jmp LOADSEG:0

; --- helpers -----------------------------------------------------------------

; eax = cluster number -> eax = its first sector (absolute LBA). Clobbers edx.
cluster_lba:
    sub eax, 2
    movzx edx, byte [bp + 0x0d]
    mul edx
    add eax, [bp - 4]
    ret

; eax = cluster number -> eax = the next cluster in its chain, from the first
; FAT. 128 entries per 512-byte sector; the sector stays in FATBUF between
; calls, so a chain that runs through one sector costs one read. Preserves ecx.
fat_next:
    push es
    push eax
    shr eax, 7
    movzx edx, word [bp + 0x0e]
    add eax, edx
    add eax, [bp + 0x1c]
    cmp eax, [bp - 8]
    je .cached
    mov [bp - 8], eax
    xor bx, bx
    mov es, bx
    mov bx, FATBUF
    call read
.cached:
    pop ebx
    and bx, 127
    shl bx, 2
    mov eax, [bx + FATBUF]
    and eax, 0x0fffffff
    pop es
    ret

; Read one sector, LBA eax, to es:bx. Returns with eax + 1 and bx + 512, every
; other register preserved. The disk address packet is built on the stack.
read:
    pushad
    push dword 0                    ; LBA, high
    push eax                        ; LBA, low
    push es                         ; buffer segment
    push bx                         ; buffer offset
    push word 1                     ; sectors
    push word 0x10                  ; packet size
    mov si, sp
    mov dl, [bp + 0x40]
    mov ah, 0x42
    int 0x13
    add sp, 16                      ; drop the packet without trusting SI to
    jc disk_error                   ; have survived the BIOS
    popad
    inc eax
    add bx, 512
    ret

; --- failure -----------------------------------------------------------------
; Say which: a stick that halts in silence cannot be told from a dead one.

not_found:
    mov si, msg_notfound
    jmp short fail
disk_error:
    mov si, msg_disk
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

name:           db "BOOTMGR    "    ; 8.3, as the directory stores it
msg_notfound:   db "Sirius: BOOTMGR not found", 0
msg_disk:       db "Sirius: disk error", 0

times 510 - ($ - $$) db 0
dw 0xaa55
