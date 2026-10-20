#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
# Check the complete first-party workspace using its one locked graph.
set -eu
cargo fmt --all -- --check
cargo clippy --workspace --all-features --all-targets --locked --offline -- -D warnings -W clippy::pedantic
cargo test --workspace --all-features --locked --offline
