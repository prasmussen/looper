#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

cargo build --release

mkdir -p ~/.local/bin
# Copy then rename, so this also works while an old looper is still running
# (overwriting a running binary in place fails with "Text file busy").
cp target/release/looper ~/.local/bin/.looper.tmp
mv -f ~/.local/bin/.looper.tmp ~/.local/bin/looper
echo "installed $HOME/.local/bin/looper"
