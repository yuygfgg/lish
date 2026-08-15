#!/bin/sh
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY

if grep -qw 'rv64.network=wsproxy' /proc/cmdline; then
    export HTTPS_PROXY=http://10.0.2.4:3128
    export https_proxy="$HTTPS_PROXY"
fi
