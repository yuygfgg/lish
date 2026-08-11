#!/bin/sh
# Print sha256sum-compatible records on macOS or Linux.
set -eu

[ "$#" -gt 0 ] || {
    echo "usage: $0 FILE..." >&2
    exit 2
}

hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        echo "missing required command: sha256sum or shasum" >&2
        exit 2
    fi
}

for file in "$@"; do
    printf '%s  %s\n' "$(hash_file "$file")" "$file"
done
