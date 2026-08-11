#!/usr/bin/env bash
# Build freestanding RV64 Linux benchmarks for the release guest image.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$root/target/guest-benchmarks}"
mkdir -p "$out"

if command -v zig >/dev/null 2>&1; then
    compiler=(zig cc -target riscv64-linux-musl)
    architecture=(-mcpu=generic_rv64+m+a+f+d+c)
elif command -v riscv64-linux-musl-gcc >/dev/null 2>&1; then
    compiler=(riscv64-linux-musl-gcc)
    architecture=(-march=rv64gc -mabi=lp64d)
elif command -v "${RISCV_PREFIX:-riscv64-none-elf-}gcc" >/dev/null 2>&1; then
    compiler=("${RISCV_PREFIX:-riscv64-none-elf-}gcc")
    architecture=(-march=rv64gc -mabi=lp64d)
elif command -v clang >/dev/null 2>&1; then
    compiler=(clang --target=riscv64-linux-gnu)
    architecture=(-march=rv64gc -mabi=lp64d)
else
    echo "install zig, clang, or an RV64 GCC cross compiler" >&2
    exit 2
fi

"${compiler[@]}" \
    "${architecture[@]}" \
    -O2 -static -nostdlib -ffreestanding -fno-stack-protector \
    -fno-pic -no-pie -Wl,--build-id=none -Wl,-e,_start -Wl,-s \
    "$root/benchmarks/jit-module-churn.c" \
    -o "$out/rv64-jit-bench"

echo "built $out/rv64-jit-bench"
