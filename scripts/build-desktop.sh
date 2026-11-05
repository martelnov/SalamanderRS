#!/bin/sh
# Builds GTK, TUI and automatic launcher from the shared Rust core.
set -eu
cd "$(dirname "$0")/.."
cargo build --release -p salamander-rs -p salamander-tui -p salamander-launcher
printf '\nDesktopové binárky:\n'
ls -lh target/release/salamander target/release/salamander-rs target/release/salamander-rs-tui
