#!/usr/bin/env sh
# Build Still and install it for the current user (~/.local).
set -e
cd "$(dirname "$0")"
cargo build --release
install -Dm755 target/release/still "$HOME/.local/bin/still"
install -Dm644 still.desktop "$HOME/.local/share/applications/still.desktop"
echo "Installed: ~/.local/bin/still (make sure ~/.local/bin is on your PATH)"
