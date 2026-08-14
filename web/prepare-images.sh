#!/usr/bin/env bash
# Prepare the Alpine/Linux browser-demo assets.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/web/images/alpine"
bench="$root/target/bench"
mkdir -p "$out" "$bench"

"$root/tools/fetch-kernel.sh" "$out/Image"

# The product path uses direct Linux boot. Keep firmware out of the default
# asset directory so an absent OpenSBI file cannot select a different path.
rm -f "$out/opensbi.bin"

if [ ! -f "$bench/alpine-riscv64.ext4" ] || \
   [ ! -f "$bench/alpine-riscv64-native.ext4" ] || \
   [ ! -f "$bench/alpine-image-v8" ]; then
    "$root/tests/virt-smoke/mk-alpine-rootfs.sh" "$bench"
fi
ln -sfn "../../../target/bench/alpine-riscv64.ext4" "$out/alpine.ext4"
ln -sfn "../../../target/bench/alpine-riscv64-native.ext4" "$out/alpine-native.ext4"

echo "demo images ready in $out"
ls -lh "$out/Image" "$out/alpine.ext4" "$out/alpine-native.ext4"
