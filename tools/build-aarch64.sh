#!/bin/sh
# Build the device (aarch64) binary for unisoc-cpd.
#
# The reliable cross target is aarch64-unknown-linux-musl: a fully static
# binary does not depend on the glibc version on the other side, which is also
# the cleanest reading of the plan's "same binary, second platform".
#
#   1. host cross build (preferred)
#          rustup target add aarch64-unknown-linux-musl
#          tools/build-aarch64.sh
#   2. build on the device, when the host has no cross toolchain
#          tools/build-aarch64.sh --on-device USER@HOST
#      (the handset's own rootfs is Debian trixie arm64; `apt install rustc cargo`)
#
# The linker is the one non-obvious part and it is handled below: for a
# *-linux-musl target rustc links the self-contained musl objects itself, but
# whatever it drives still has to understand AArch64 options -- the host's GNU
# ld rejects `--fix-cortex-a53-843419`, which is an lld option.  rust-lld ships
# inside the toolchain, so we point the target at it rather than requiring a
# cross gcc.
set -eu

cd "$(dirname "$0")/.."
TARGET=aarch64-unknown-linux-musl
OUT=target/$TARGET/release/unisoc-cpd
LINKER_VAR=CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER

if [ "${1:-}" = "--on-device" ]; then
    remote=${2:?usage: build-aarch64.sh --on-device USER@HOST}
    echo "copying the tree to $remote and building there"
    tar -cf - --exclude target --exclude runs . | ssh "$remote" '
        set -eu
        rm -rf ~/unisoc-cpd && mkdir -p ~/unisoc-cpd && cd ~/unisoc-cpd && tar -xf -
        command -v cargo >/dev/null || { echo "no cargo on the device: apt install rustc cargo" >&2; exit 1; }
        cargo build --release
        mv target/release/unisoc-cpd ./unisoc-cpd.aarch64
    '
    scp "$remote:~/unisoc-cpd/unisoc-cpd.aarch64" ./unisoc-cpd.aarch64
    echo "built ./unisoc-cpd.aarch64"
    exit 0
fi

if rustc --print sysroot >/dev/null 2>&1; then
    sysroot=$(rustc --print sysroot)
    if [ -d "$sysroot/lib/rustlib/$TARGET/lib" ]; then
        host=$(rustc -vV | sed -n 's/^host: //p')
        lld="$sysroot/lib/rustlib/$host/bin/rust-lld"
        if [ -x "$lld" ]; then
            eval "export $LINKER_VAR='$lld'"
            echo "linking with $lld"
        else
            echo "warning: no rust-lld in $sysroot; falling back to the default linker," >&2
            echo "         which may not understand AArch64 options" >&2
        fi
        cargo build --release --target "$TARGET"
        echo "built $OUT"
        file "$OUT" 2>/dev/null || true
        exit 0
    fi
fi

cat >&2 <<EOF
No $TARGET standard library for this rustc.

Install it and re-run:

    rustup target add $TARGET
    tools/build-aarch64.sh

Or build where the toolchain already matches the device:

    tools/build-aarch64.sh --on-device USER@HOST

A dynamically linked aarch64 build against this host's glibc is deliberately
not attempted: it would carry the host's glibc requirement to a Debian trixie
device, which is exactly the kind of hidden coupling the plan's portability
gate is meant to keep out.
EOF
exit 1
