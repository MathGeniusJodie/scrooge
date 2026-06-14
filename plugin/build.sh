#!/usr/bin/env bash
# Build the scrooge binary and stage it into the plugin's bin/ directory.
# Uses an atomic rename so it works even while the MCP server is running (a
# plain `cp` fails with "Text file busy" because the live process holds the
# file open). To also restart the server afterward, use stage.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
mkdir -p plugin/bin
cp target/release/scrooge plugin/bin/scrooge.new
mv -f plugin/bin/scrooge.new plugin/bin/scrooge
echo "staged plugin/bin/scrooge ($(du -h plugin/bin/scrooge | cut -f1))"
