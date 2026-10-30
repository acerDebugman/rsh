#!/usr/bin/env bash
# Development environment for rsh.
#
# The machine's cargo config replaces crates.io with an internal mirror, but
# public hosts resolve to IPv6 first and IPv6 has no route here, so cargo
# hangs. Start a local IPv4-only CONNECT proxy and point cargo at it.
#
# Usage:  source scripts/dev-env.sh
#         cargo build
#
# This is development tooling only; the rsh binary itself never uses it.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${RSH_CARGO_PROXY_PORT:-18080}"
PROXY_PID=""

if ! curl -sS -o /dev/null --max-time 2 \
    -x "http://127.0.0.1:${PORT}" https://index.crates.io/config.json 2>/dev/null; then
    mkdir -p "$ROOT/.tmp"
    python3 "$ROOT/scripts/ipv4_connect_proxy.py" "$PORT" \
        >"$ROOT/.tmp/cargo-proxy.log" 2>&1 &
    PROXY_PID=$!
    sleep 0.5
    if ! kill -0 "$PROXY_PID" 2>/dev/null; then
        echo "dev-env: proxy failed to start; see .tmp/cargo-proxy.log" >&2
        return 1 2>/dev/null || exit 1
    fi
    echo "dev-env: started IPv4 proxy on 127.0.0.1:${PORT} (pid ${PROXY_PID})"
else
    echo "dev-env: reusing proxy on 127.0.0.1:${PORT}"
fi

export CARGO_HTTP_PROXY="http://127.0.0.1:${PORT}"
export HTTPS_PROXY="http://127.0.0.1:${PORT}"
export HTTP_PROXY="http://127.0.0.1:${PORT}"
export NO_PROXY="127.0.0.1,localhost"
# Keep cargo's home inside the workspace when the session is sandboxed
# (writes to ~/.cargo are denied); remove this line for a normal shell.
export CARGO_HOME="${RSH_CARGO_HOME:-$ROOT/.tmp/cargo-home}"
# The sandbox makes /home/algo/bigtmp (the machine TMPDIR) read-only; keep
# rustc/rustdoc scratch files inside the workspace.
export TMPDIR="${RSH_TMPDIR:-$ROOT/target/tmp}"
mkdir -p "$TMPDIR"

echo "dev-env: CARGO_HTTP_PROXY=${CARGO_HTTP_PROXY} CARGO_HOME=${CARGO_HOME} TMPDIR=${TMPDIR}"
