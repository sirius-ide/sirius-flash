#!/usr/bin/env bash
# Build a UEFI-bootable Windows 11 installer USB (GPT + FAT32 + split install.wim)
# Target is addressed ONLY via /dev/disk/by-id serial path, with asserts before any write.
set -euo pipefail

DEV="/dev/disk/by-id/usb-Kingston_DataTraveler_3.0_60A44C3FAD9EFEB1398E00D2-0:0"
ISO="/run/media/emrys/DATA8TB/Win11_25H2_English_x64_v2.iso"
LOG="/tmp/w11usb.log"

exec > >(tee "$LOG") 2>&1
echo "=== $(date) starting ==="

# ---------- safety asserts ----------
[[ -e "$DEV" ]] || { echo "ABORT: by-id device not found: $DEV"; exit 1; }
REAL=$(readlink -f "$DEV")
BASE=$(basename "$REAL")
REMOVABLE=$(cat "/sys/block/$BASE/removable")
SIZE=$(blockdev --getsize64 "$REAL")
MODEL=$(lsblk -dno MODEL "$REAL" | xargs)
echo "Resolved: $DEV -> $REAL  model='$MODEL'  size=$SIZE  removable=$REMOVABLE"

[[ "$REMOVABLE" == "1" ]]                 || { echo "ABORT: not a removable device"; exit 1; }
[[ "$MODEL" == *"DataTraveler"* ]]        || { echo "ABORT: model mismatch"; exit 1; }
(( SIZE > 50000000000 && SIZE < 70000000000 )) || { echo "ABORT: size outside 50-70GB window"; exit 1; }
[[ "$REAL" != "$(readlink -f /dev/disk/by-id/*ST8000NE001*WKD120Z9* 2>/dev/null || echo none)" ]] \
                                          || { echo "ABORT: resolved to the 8TB Seagate!"; exit 1; }
[[ -f "$ISO" ]]                           || { echo "ABORT: ISO not found"; exit 1; }

# ---------- install needed packages ----------
if ! command -v mkfs.fat >/dev/null || ! command -v wimlib-imagex >/dev/null; then
  echo "Installing dosfstools + wimlib..."
  pacman -S --noconfirm --needed dosfstools wimlib || { pacman -Sy --noconfirm --needed dosfstools wimlib; }
fi
command -v mkfs.fat >/dev/null       || { echo "ABORT: mkfs.fat still missing"; exit 1; }
command -v wimlib-imagex >/dev/null  || { echo "ABORT: wimlib-imagex still missing"; exit 1; }

# ---------- unmount, partition, format ----------
for p in "$DEV"-part*; do
  [[ -e "$p" ]] && mountpoint -q "$(lsblk -no MOUNTPOINT "$(readlink -f "$p")" | head -1)" 2>/dev/null && umount "$p" && echo "unmounted $p" || true
done
umount "${DEV}-part1" 2>/dev/null || true

echo "Creating GPT + FAT32 partition on $DEV ..."
parted --script "$DEV" mklabel gpt mkpart WIN11 fat32 1MiB 100% set 1 msftdata on
udevadm settle
mkfs.fat -F 32 -n WIN11USB "${DEV}-part1"

# ---------- mount ISO and USB ----------
mkdir -p /run/w11iso /run/w11usb
mount -o loop,ro "$ISO" /run/w11iso
mount "${DEV}-part1" /run/w11usb
trap 'cd /; umount /run/w11usb 2>/dev/null; umount /run/w11iso 2>/dev/null' EXIT

# ---------- copy everything except install.wim ----------
echo "Copying ISO contents (except install.wim)..."
rsync -rt --no-perms --no-owner --no-group --exclude=sources/install.wim /run/w11iso/ /run/w11usb/

# ---------- split install.wim into <4GB chunks ----------
echo "Splitting install.wim into .swm chunks..."
wimlib-imagex split /run/w11iso/sources/install.wim /run/w11usb/sources/install.swm 3800

# ---------- verify ----------
echo "Verifying boot files..."
[[ -f /run/w11usb/efi/boot/bootx64.efi ]] || { echo "ABORT: efi/boot/bootx64.efi missing on USB"; exit 1; }
[[ -f /run/w11usb/sources/boot.wim ]]     || { echo "ABORT: sources/boot.wim missing on USB"; exit 1; }
ls -lh /run/w11usb/sources/install*.swm
echo "Syncing (this can take a while on USB)..."
sync
df -h /run/w11usb
umount /run/w11usb
umount /run/w11iso
trap - EXIT
echo "=== DONE OK $(date) ==="
