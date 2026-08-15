#!/usr/bin/env bash
# Build the release Alpine image as an unprivileged user on macOS or Linux.
set -euo pipefail

OUT="${1:-$PWD}"
VERSION="${ALPINE_VERSION:-3.24.1}"
EXPECTED_SHA256="${ALPINE_SHA256:-7201513262d851f39105102cf95519410100259bd7996fca13bade517838d7b7}"
NATIVE_IMAGE_SIZE_MIB=2048
BRANCH="v${VERSION%.*}"
MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}"
WEB_IMG="$OUT/alpine-riscv64.ext4"
NATIVE_IMG="$OUT/alpine-riscv64-native.ext4"
IMAGE_STAMP="$OUT/alpine-image-v9"
BENCH="$OUT/guest-benchmarks/rv64-jit-bench"
TARBALL="$OUT/alpine-minirootfs-$VERSION-riscv64.tar.gz"
URL="$MIRROR/$BRANCH/releases/riscv64/$(basename "$TARBALL")"
APK_REPOSITORY="$MIRROR/$BRANCH/main/riscv64"
ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"

WGET_APK="$OUT/wget-1.25.0-r3.apk"
WGET_SHA256="f9d7afcf4bc7f2fd61876a934613c3fb7cf1081984d8135eefeece784e1df0fd"
LIBIDN2_APK="$OUT/libidn2-2.3.8-r0.apk"
LIBIDN2_SHA256="f7969149f59dae73510cc721c50368e3841dab82593f8f79b05a532be27ea3c4"
LIBUNISTRING_APK="$OUT/libunistring-1.4.2-r0.apk"
LIBUNISTRING_SHA256="bdec9670c7e516493e23d0e7e145db1380cbce4dc46949b837961b25bc4cc81d"
PCRE2_APK="$OUT/pcre2-10.47-r1.apk"
PCRE2_SHA256="4a0d802e350db6a15b951371468ff1f7172dcee42928153b1e6367459467eb8c"

require() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing required command: $1" >&2
        exit 2
    }
}

fetch_checked() {
    local destination="$1"
    local url="$2"
    local expected_sha256="$3"
    local description="$4"
    local actual_sha256

    if [ ! -f "$destination" ]; then
        curl --fail --location --retry 3 --output "$destination.part" "$url"
        mv "$destination.part" "$destination"
    fi

    actual_sha256="$("$ROOT_DIR/tools/sha256.sh" "$destination" | cut -d ' ' -f 1)"
    if [ "$actual_sha256" != "$expected_sha256" ]; then
        echo "$description checksum mismatch" >&2
        echo "expected: $expected_sha256" >&2
        echo "actual:   $actual_sha256" >&2
        exit 1
    fi
}

extract_apk() {
    local archive="$1"
    local destination="$2"

    mkdir -p "$destination"
    gzip -dc "$archive" | tar --ignore-zeros -xf - -C "$destination"
}

require curl
require cut
require du
require genext2fs
require gzip
require tar
require wc

assemble_image() {
    local image="$1"
    local size_mib="$2"
    local blocks=$(( size_mib * 256 ))

    # Keep the historical ext4 suffix because it is part of the asset protocol.
    # The image uses ext2 so an unprivileged, cross-platform tool can build it.
    genext2fs \
        -B 4096 \
        -b "$blocks" \
        -i 16384 \
        -m 0 \
        -L rv64-alpine \
        -a "$base_tar" \
        -U -d "$overlay" \
        "$image"

    echo "assembled $image (${size_mib} MiB, Alpine $VERSION riscv64, ext2)"
}

mkdir -p "$OUT"
rm -f "$IMAGE_STAMP"
fetch_checked "$TARBALL" "$URL" "$EXPECTED_SHA256" "Alpine minirootfs"
fetch_checked \
    "$WGET_APK" "$APK_REPOSITORY/$(basename "$WGET_APK")" \
    "$WGET_SHA256" "GNU Wget APK"
fetch_checked \
    "$LIBIDN2_APK" "$APK_REPOSITORY/$(basename "$LIBIDN2_APK")" \
    "$LIBIDN2_SHA256" "libidn2 APK"
fetch_checked \
    "$LIBUNISTRING_APK" "$APK_REPOSITORY/$(basename "$LIBUNISTRING_APK")" \
    "$LIBUNISTRING_SHA256" "libunistring APK"
fetch_checked \
    "$PCRE2_APK" "$APK_REPOSITORY/$(basename "$PCRE2_APK")" \
    "$PCRE2_SHA256" "PCRE2 APK"

work="$(mktemp -d "${TMPDIR:-/tmp}/rv64-alpine.XXXXXX")"
trap 'rm -rf "$work"' EXIT
overlay="$work/overlay"
base_tar="$work/alpine-minirootfs.tar"
packages="$work/packages"

"$ROOT_DIR/tools/build-guest-benchmarks.sh" "$OUT/guest-benchmarks"
mkdir -p \
    "$overlay/usr/lib" \
    "$overlay/usr/local/bin"
install -m 0755 "$BENCH" "$overlay/usr/local/bin/rv64-jit-bench"

extract_apk "$WGET_APK" "$packages/wget"
extract_apk "$LIBIDN2_APK" "$packages/libidn2"
extract_apk "$LIBUNISTRING_APK" "$packages/libunistring"
extract_apk "$PCRE2_APK" "$packages/pcre2"
install -m 0755 "$packages/wget/usr/bin/wget" "$overlay/usr/local/bin/wget"
cp -a "$packages/libidn2/usr/lib/." "$overlay/usr/lib/"
cp -a "$packages/libunistring/usr/lib/." "$overlay/usr/lib/"
cp -a "$packages/pcre2/usr/lib/." "$overlay/usr/lib/"

cat > "$overlay/rv64-init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY

if grep -qw 'rv64.network=wsproxy' /proc/cmdline; then
    export HTTPS_PROXY=http://10.0.2.4:3128
    export https_proxy="$HTTPS_PROXY"
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
        # The CONNECT proxy waits for the first tunnel payload before it opens
        # the upstream TCP connection. Retry transient network failures only.
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
web_image_size_mib=$(( (payload_bytes + 1048575) / 1048576 + 64 ))

assemble_image "$WEB_IMG" "$web_image_size_mib"
assemble_image "$NATIVE_IMG" "$NATIVE_IMAGE_SIZE_MIB"

touch "$IMAGE_STAMP"
