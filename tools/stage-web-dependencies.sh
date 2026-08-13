#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
source_dir="$root/node_modules/@xterm"
target_dir="$root/web/vendor/xterm"

if [ ! -f "$source_dir/xterm/lib/xterm.mjs" ] || \
   [ ! -f "$source_dir/addon-fit/lib/addon-fit.mjs" ]; then
    echo "xterm dependencies are missing; run npm ci" >&2
    exit 2
fi

mkdir -p "$target_dir"
install -m 0644 "$source_dir/xterm/lib/xterm.mjs" "$target_dir/xterm.js"
install -m 0644 "$source_dir/xterm/css/xterm.css" "$target_dir/xterm.css"
install -m 0644 "$source_dir/addon-fit/lib/addon-fit.mjs" "$target_dir/addon-fit.js"
