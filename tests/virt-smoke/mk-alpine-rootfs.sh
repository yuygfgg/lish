#!/usr/bin/env bash
# Build the release Alpine image as an unprivileged user on macOS or Linux.
set -euo pipefail

OUT="${1:-$PWD}"
VERSION="${ALPINE_VERSION:-3.24.1}"
EXPECTED_SHA256="${ALPINE_SHA256:-7201513262d851f39105102cf95519410100259bd7996fca13bade517838d7b7}"
BRANCH="v${VERSION%.*}"
MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}"
IMG="$OUT/alpine-riscv64.ext4"
BENCH="$OUT/guest-benchmarks/rv64-jit-bench"
TARBALL="$OUT/alpine-minirootfs-$VERSION-riscv64.tar.gz"
URL="$MIRROR/$BRANCH/releases/riscv64/$(basename "$TARBALL")"
ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"

require() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing required command: $1" >&2
        exit 2
    }
}

require curl
require cut
require du
require genext2fs
require gzip
require wc

mkdir -p "$OUT"
if [ ! -f "$TARBALL" ]; then
    curl --fail --location --retry 3 --output "$TARBALL" "$URL"
fi
actual_sha256="$("$ROOT_DIR/tools/sha256.sh" "$TARBALL" | cut -d ' ' -f 1)"
if [ "$actual_sha256" != "$EXPECTED_SHA256" ]; then
    echo "Alpine minirootfs checksum mismatch" >&2
    echo "expected: $EXPECTED_SHA256" >&2
    echo "actual:   $actual_sha256" >&2
    exit 1
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/rv64-alpine.XXXXXX")"
trap 'rm -rf "$work"' EXIT
overlay="$work/overlay"
base_tar="$work/alpine-minirootfs.tar"

"$ROOT_DIR/tools/build-guest-benchmarks.sh" "$OUT/guest-benchmarks"
mkdir -p \
    "$overlay/usr/local/bin"
install -m 0755 "$BENCH" "$overlay/usr/local/bin/rv64-jit-bench"

cat > "$overlay/rv64-init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY

if grep -qw 'rv64.network=wsproxy' /proc/cmdline; then
    ip link set eth0 up
    if udhcpc -i eth0 -q -n -t 5; then
        echo LISH_NETWORK_DHCP=OK
        if env | grep -qiE '(^|_)https?_proxy='; then
            echo LISH_NETWORK_PROXY=ON
        else
            echo LISH_NETWORK_PROXY=OFF
        fi
        if nslookup dl-cdn.alpinelinux.org 10.0.2.3 >/dev/null 2>&1; then
            echo LISH_NETWORK_DNS=OK
        else
            echo LISH_NETWORK_DNS=FAIL
        fi
        # The first TLS workload can compile cold guest code. Fastly closes an
        # idle TCP connection after about one second, so retry the same HTTPS
        # path during boot before declaring the network unhealthy.
        https_ok=0
        for _ in 1 2 3 4 5; do
            if wget -q -T 3 -t 1 -O /dev/null \
                https://dl-cdn.alpinelinux.org/alpine/; then
                https_ok=1
                break
            fi
        done
        if [ "$https_ok" -eq 1 ]; then
            echo LISH_NETWORK_HTTPS=OK
        else
            echo LISH_NETWORK_HTTPS=FAIL
        fi
    else
        echo LISH_NETWORK_DHCP=FAIL
    fi
fi

hostname rv64
echo ALPINE_READY
echo 'Try: apk update && apk add nano'
echo 'JIT lifecycle benchmark: rv64-jit-bench [pages] [rounds] [calls]'
exec /bin/sh -l
EOF
chmod 0755 "$overlay/rv64-init"

# genext2fs reads an uncompressed tar archive and preserves its target metadata.
# It applies the overlay as a second layer and maps host ownership to root.
gzip -dc "$TARBALL" > "$base_tar"
payload_bytes=$(( $(wc -c < "$base_tar") + $(du -sk "$overlay" | cut -f 1) * 1024 ))
size_mb=$(( (payload_bytes + 1048575) / 1048576 + 64 ))
blocks=$(( size_mb * 256 ))

# Keep the historical file name because it is part of the demo asset protocol.
# The image uses ext2 so that an unprivileged, cross-platform tool can build it.
genext2fs \
    -B 4096 \
    -b "$blocks" \
    -i 16384 \
    -m 0 \
    -L rv64-alpine \
    -a "$base_tar" \
    -U -d "$overlay" \
    "$IMG"

touch "$OUT/alpine-image-v6"
echo "assembled $IMG (${size_mb} MiB, Alpine $VERSION riscv64, ext2)"
