#!/bin/sh
# Copy only the launcher and TUI. PREFIX=/usr/local (default), custom prefix permitted.
set -eu
cd "$(dirname "$0")/.."
prefix=${PREFIX:-/usr/local}
[ -f target/release/salamander ] && [ -f target/release/salamander-rs-tui ] || {
    echo 'Nejprve spusťte scripts/build-server.sh' >&2
    exit 1
}
install -d "$prefix/bin"
install -m 0755 target/release/salamander "$prefix/bin/salamander"
install -m 0755 target/release/salamander-rs-tui "$prefix/bin/salamander-rs-tui"
echo "Hotovo: $prefix/bin/salamander (bez GTK)"
