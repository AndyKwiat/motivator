#!/usr/bin/env bash
# Build for Linux and deploy to motivate.akwiat.com via radify.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --target x86_64-unknown-linux-musl
rm -rf dist && mkdir -p dist/bin
cp target/x86_64-unknown-linux-musl/release/app dist/bin/app
cp -R static dist/static
"${RADIFY:-$HOME/radify/bin/radify}" deploy motivate dist
