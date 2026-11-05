#!/bin/sh
# Native Linux/SSH build: do NOT compile GTK or install a graphical environment.
set -eu
cd "$(dirname "$0")/.."
if ! command -v cargo >/dev/null 2>&1; then
    echo 'Chybí Rust/Cargo (rustup.rs, Rust 1.85+).' >&2
    exit 1
fi
cargo build --release -p salamander-tui -p salamander-launcher
printf '\nServerové binárky:\n'
ls -lh target/release/salamander target/release/salamander-rs-tui
if command -v ldd >/dev/null 2>&1; then
    printf '\nDynamické závislosti TUI:\n'
    ldd target/release/salamander-rs-tui
    if ldd target/release/salamander-rs-tui | grep -E 'libgtk|libgdk|libX11|libwayland|libncurses'; then
        echo 'CHYBA: TUI je závislé na GUI/ncurses!' >&2
        exit 1
    fi
fi
printf '\nSpuštění na tomto počítači: ./target/release/salamander --tui\n'
