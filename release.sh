#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

cargo build --release

mkdir -p ~/.local/bin
cp target/release/looper ~/.local/bin/looper
echo "installed $HOME/.local/bin/looper"
