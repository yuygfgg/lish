#!/usr/bin/env bash
# Lish virt-machine smoke and regression test.
#
# Boots the modern "virt" machine with a stock riscv64 Linux kernel and a tiny
# initramfs whose init (init.c) exercises the full-system
# paths that were once broken: the 8250 THRE transmit interrupt, LR/SC
# reservation handling across traps, and a live-advancing rdtime. On success
# the guest prints SMOKE_OK and powers off; a hang (missing THRE interrupt,
# etc.) makes this script time out and FAIL.
#
# Set RV64_MODERN_KERNEL to a compatible kernel Image. The default is the
# versioned image downloaded by web/prepare-images.sh. Set RISCV_PREFIX to a
# bare-metal RISC-V compiler, or use the Zig toolchain.
#
# Usage:  tests/virt-smoke/run.sh
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$root"
CARGO="$root/tools/cargo"

image="${RV64_MODERN_KERNEL:-$root/web/images/alpine/Image}"
cc="${RISCV_PREFIX:-riscv64-none-elf-}gcc"

[ -s "$image" ] || {
  echo "[virt-smoke] FAIL: missing kernel=$image; run web/prepare-images.sh"; exit 2; }

if command -v "$cc" >/dev/null 2>&1; then
  compiler=("$cc")
  architecture=(-march=rv64gc -mabi=lp64d)
elif command -v zig >/dev/null 2>&1; then
    compiler=(zig cc -target riscv64-linux-musl)
    architecture=(-mcpu=generic_rv64+m+a+f+d+c)
    export ZIG_GLOBAL_CACHE_DIR="$work/zig-cache"
else
  echo "[virt-smoke] FAIL: install Zig or set RISCV_PREFIX" >&2
  exit 2
fi

echo "[virt-smoke] building rv64-vboot…"
"$CARGO" build --release --bin rv64-vboot >/dev/null 2>&1

echo "[virt-smoke] building guest init + initramfs…"
# Freestanding (no libc): a static, non-PIE riscv64 ELF Linux can load and run
# directly. Works with any riscv64 gcc (bare-metal or linux cross).
"${compiler[@]}" -nostdlib -ffreestanding -static -no-pie \
    "${architecture[@]}" -Os \
    -Wl,-T,"$here/link.ld" -o "$work/init" "$here/init.c"
mkdir -p "$work/irfs"
cp "$work/init" "$work/irfs/init"
if command -v cpio >/dev/null 2>&1; then
  cpio_cmd=(cpio)
elif [ -x /opt/homebrew/opt/libarchive/bin/cpio ]; then
  cpio_cmd=(/opt/homebrew/opt/libarchive/bin/cpio)
else
  echo "[virt-smoke] FAIL: install cpio (Homebrew: libarchive)" >&2
  exit 2
fi
( cd "$work/irfs" && find . | "${cpio_cmd[@]}" -o -H newc 2>/dev/null | gzip ) > "$work/initramfs.cpio.gz"

echo "[virt-smoke] booting virt machine…"
out="$work/out.log"
if command -v timeout >/dev/null 2>&1; then
  timeout_cmd=(timeout)
elif command -v gtimeout >/dev/null 2>&1; then
  timeout_cmd=(gtimeout)
else
  echo "[virt-smoke] FAIL: install timeout or coreutils" >&2
  exit 2
fi
VBOOT_MAX_INSNS=9000000000000000 "${timeout_cmd[@]}" 120 \
  "$root/target/release/rv64-vboot" --direct "$image" \
  --initrd "$work/initramfs.cpio.gz" --ram 0.25 \
  -- "console=ttyS0 earlycon=uart8250,mmio,0x10000000 rdinit=/init" \
  < /dev/null > "$out" 2>&1 || true

echo "[virt-smoke] guest markers:"
grep -aE 'SMOKE_START|RDTIME_OK|RTC_OK|TTY_DRAIN_OK|FORKS_OK|SMOKE_OK|FAIL_' "$out" | sed 's/^/    /' || true

if grep -qa 'SMOKE_OK' "$out" && grep -qa 'RTC_OK' "$out" && ! grep -qa 'FAIL_' "$out"; then
  echo "[virt-smoke] PASS"
  exit 0
else
  echo "[virt-smoke] FAIL — guest did not reach SMOKE_OK (hang or error). Last output:"
  tail -15 "$out" | sed 's/^/    /'
  exit 1
fi
