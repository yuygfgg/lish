#!/usr/bin/env bash
# Lish validation suite. Run from anywhere; stages with external tools or
# images report a skip. Set REQUIRE_ALL=1 to make every skip a failure.
set -euo pipefail

cd "$(dirname "$0")/.."
FAILED=0
CARGO="$PWD/tools/cargo"

note() { printf '\n=== %s\n' "$*"; }
skip() {
    echo "SKIP ($*)"
    if [ "${REQUIRE_ALL:-0}" = 1 ]; then FAILED=1; fi
}
run_stage() {
    if "$@"; then
        return 0
    fi
    FAILED=1
}

note "1/6 Rust workspace"
run_stage "$CARGO" test --workspace --exclude rv64-wasm --release -q
run_stage "$CARGO" test -p rv64-wasm --lib --release -q
run_stage "$CARGO" clippy --workspace --all-targets -- -D warnings

note "2/6 architecture validation"
PREFIX="${RISCV_PREFIX:-riscv64-none-elf-}"
if command -v "${PREFIX}gcc" >/dev/null 2>&1; then
    run_stage env RISCV_PREFIX="$PREFIX" tests/run-isa-tests.sh
else
    skip "${PREFIX}gcc not found; set RISCV_PREFIX"
fi

if command -v spike >/dev/null 2>&1 && [ -d tests/riscv-tests/isa ]; then
    isa_binaries=()
    while IFS= read -r binary; do
        isa_binaries+=("$binary")
    done < <(find tests/riscv-tests/isa -type f \( \
        -name 'rv64u[imac]-p-*' -o -name 'rv64ud-p-*' -o -name 'rv64uf-p-*' \
    \) ! -name '*dump' | sort)
    if [ "${#isa_binaries[@]}" -gt 0 ]; then
        run_stage python3 tests/lockstep.py "${isa_binaries[@]}"
    else
        skip "riscv-tests binaries are missing; run tests/run-isa-tests.sh first"
    fi
else
    skip "spike or riscv-tests binaries are missing"
fi

if command -v spike >/dev/null 2>&1 && command -v "${PREFIX}gcc" >/dev/null 2>&1; then
    run_stage tests/run-arch-tests.sh
else
    skip "spike or ${PREFIX}gcc not found"
fi

note "3/6 Wasm runtime and JIT"
if command -v node >/dev/null 2>&1; then
    run_stage "$CARGO" build --release -q -p rv64-wasm --target wasm32-unknown-unknown
    run_stage node tests/boot-profile-selftest.mjs
    run_stage node tests/jit-code-store.mjs
    run_stage node tests/native-disk.mjs
    if [ -f target/wasm32-unknown-unknown/release/rv64_wasm.wasm ]; then
        run_stage node tests/host-callback-boundary.mjs
        run_stage node tests/public-api.mjs
        run_stage node tests/virt-jit.mjs
        run_stage node tests/worker-api.mjs
    else
        skip "Wasm build did not produce rv64_wasm.wasm"
    fi
else
    skip "node not found"
fi

note "4/6 Alpine direct boot"
if command -v node >/dev/null 2>&1; then
    if [ -f target/wasm32-unknown-unknown/release/rv64_wasm.wasm \
        ] && [ -f web/images/alpine/Image ] && [ -f web/images/alpine/alpine.ext4 ]; then
        run_stage node tests/alpine-boot.mjs
    else
        skip "Wasm, Alpine kernel, or Alpine disk is missing; run web/prepare-images.sh"
    fi
else
    skip "node not found"
fi

note "5/6 Native network host"
if command -v swift >/dev/null 2>&1; then
    run_stage swift test --package-path native -Xswiftc -warnings-as-errors
else
    skip "swift not found"
fi

note "6/6 VirtMachine native smoke"
if [ -s web/images/alpine/Image ] && \
    { command -v zig >/dev/null 2>&1 || command -v "${PREFIX}gcc" >/dev/null 2>&1; }; then
    run_stage tests/virt-smoke/run.sh
else
    skip "kernel or Zig/RISC-V compiler is missing; run web/prepare-images.sh"
fi

printf '\n'
if [ "$FAILED" -eq 0 ]; then
    echo "ALL STAGES PASSED"
else
    echo "SUITE FAILED"
fi
exit "$FAILED"
