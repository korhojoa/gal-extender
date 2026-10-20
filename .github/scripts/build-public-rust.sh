#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
# Compile source only. Do not connect to a device or issue release versions.
set -eu
started=$(date +%s)
TARGET=armv7-unknown-nto-qnx650
rustc --print target-list | grep -qx "$TARGET"
cargo clippy --workspace --all-features --lib --locked --offline --target "$TARGET" -- -D warnings -W clippy::pedantic
# Rebuild the fixed-name cdylib when this cache has served another checkout.
# Keep all dependency artifacts in the shared target directory.
cargo clean -p gal-loader --release --target "$TARGET"
cargo build --workspace --all-features --lib --release --locked --offline --target "$TARGET"
file="$CARGO_TARGET_DIR/$TARGET/release/libgal_loader.so"
readelf -h "$file" | grep 'Version5 EABI'
readelf -d "$file" | grep NEEDED
if readelf -d "$file" | grep NEEDED | grep -v 'libc.so.3\|libm.so.2\|libsocket.so.3'; then
    echo 'Unexpected runtime library' >&2
    exit 1
fi
sha256sum "$file"
echo "Rust library build and ELF checks: $(($(date +%s) - started)) seconds"
