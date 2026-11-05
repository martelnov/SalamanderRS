#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p salamander-web
printf 'Hotovo: %s\n' "$(pwd)/target/release/salamander-rs-web"
