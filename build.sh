#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
ENGINE=${CONTAINER_ENGINE:-podman}
CACHE=${CARGO_HOME:-$HOME/.cargo}
TARGET_CACHE=${CARGO_TARGET_DIR:-$ROOT/target}
mkdir -p "$CACHE/registry" "$CACHE/git" "$TARGET_CACHE"
CACHE=$(CDPATH='' cd -- "$CACHE" && pwd)
TARGET_CACHE=$(CDPATH='' cd -- "$TARGET_CACHE" && pwd)
exec "$ENGINE" run --rm -v "$ROOT:/src" -v "$TARGET_CACHE:/src/target" \
  -v "$CACHE/registry:/cargo/registry" -v "$CACHE/git:/cargo/git" \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/src/target -w /src \
  --entrypoint /bin/sh "$(cat "$ROOT/compiler-image.txt")" -ec \
  'cargo fetch --locked; sh .github/scripts/check-public-rust.sh; sh .github/scripts/build-public-rust.sh'
