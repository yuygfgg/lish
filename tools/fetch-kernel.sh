#!/usr/bin/env bash
# Fetch and verify the Linux Image used by the Lish Alpine demo.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
destination="${1:-$root/web/images/alpine/Image}"
source_file="${RV64_KERNEL_FILE:-}"
url="${RV64_KERNEL_URL:-https://github.com/ibuildthecloud/rv64.js/releases/download/demo-images-v3/modern-Image}"
expected="${RV64_KERNEL_SHA256:-2d95fe4d6006d5b9975beac74e85df458bcbc76bff412baf7e718451516b7e87}"

mkdir -p "$(dirname "$destination")"
checksum() {
    "$root/tools/sha256.sh" "$1" | cut -d ' ' -f 1
}

if [ -n "$source_file" ]; then
    [ -s "$source_file" ] || {
        echo "kernel file is missing or empty: $source_file" >&2
        exit 2
    }
    actual="$(checksum "$source_file")"
    [ "$actual" = "$expected" ] || {
        echo "kernel checksum mismatch for $source_file" >&2
        echo "expected: $expected" >&2
        echo "actual:   $actual" >&2
        exit 1
    }
    install -m 0644 "$source_file" "$destination"
    exit 0
fi

if [ -s "$destination" ] && [ "$(checksum "$destination")" = "$expected" ]; then
    echo "using cached kernel $destination"
    exit 0
fi

command -v curl >/dev/null 2>&1 || {
    echo "missing required command: curl" >&2
    exit 2
}

temporary="$(mktemp "${TMPDIR:-/tmp}/lish-kernel.XXXXXX")"
trap 'rm -f "$temporary"' EXIT
curl --fail --location --retry 3 --output "$temporary" "$url"
actual="$(checksum "$temporary")"
[ "$actual" = "$expected" ] || {
    echo "kernel checksum mismatch" >&2
    echo "url:      $url" >&2
    echo "expected: $expected" >&2
    echo "actual:   $actual" >&2
    exit 1
}
install -m 0644 "$temporary" "$destination"
echo "fetched kernel $destination"
