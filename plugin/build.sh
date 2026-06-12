#!/usr/bin/env bash
# Build the scrooge binary and stage it into the plugin's bin/ directory.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
mkdir -p plugin/bin
cp target/release/scrooge plugin/bin/scrooge
echo "staged plugin/bin/scrooge ($(du -h plugin/bin/scrooge | cut -f1))"
