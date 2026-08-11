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
    "$overlay/etc/profile.d" \
    "$overlay/run/rv64-proxy" \
    "$overlay/usr/local/bin"
install -m 0755 "$BENCH" "$overlay/usr/local/bin/rv64-jit-bench"

cat > "$overlay/etc/profile.d/rv64-proxy.sh" <<'EOF'
if grep -qw 'rv64.network=fetch' /proc/cmdline 2>/dev/null; then
    export http_proxy=http://10.0.2.2:8080
    export https_proxy=http://10.0.2.2:8080
    export HTTP_PROXY="$http_proxy"
    export HTTPS_PROXY="$https_proxy"
else
    unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY
fi
EOF

cat > "$overlay/rv64-init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
. /etc/profile.d/rv64-proxy.sh

ip link set eth0 up
ip addr add 10.0.2.15/24 dev eth0
ip route add default via 10.0.2.2

# The emulator exposes the exact ephemeral proxy CA through this private 9p
# share. The minirootfs already contains apk and the public CA bundle.
if grep -qw 'rv64.network=fetch' /proc/cmdline && \
   mount -t 9p -o trans=virtio,version=9p2000.L,ro rv64-proxy /run/rv64-proxy; then
    ca_bundle=/etc/ssl/certs/ca-certificates.crt
    ca_base=/etc/ssl/certs/ca-certificates.rv64-base.crt
    if [ ! -s "$ca_base" ]; then
        cp "$ca_bundle" "$ca_base"
    fi
    if [ -s /run/rv64-proxy/ca.pem ] &&
       cat "$ca_base" /run/rv64-proxy/ca.pem > /run/rv64-ca-bundle.crt &&
       mv /run/rv64-ca-bundle.crt "$ca_bundle"; then
        echo PROXY_CA_READY
    fi
fi

hostname rv64
echo ALPINE_READY
echo 'Networking is configured. Try: apk update && apk add nano'
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

touch "$OUT/alpine-image-v5"
echo "assembled $IMG (${size_mb} MiB, Alpine $VERSION riscv64, ext2)"
