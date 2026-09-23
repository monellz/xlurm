#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly INSTALL_DIR="/usr/local/bin"
readonly BINARIES=(xlurm xrun xbatch xqueue xcancel xinfo)

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo is required to build xlurm" >&2
    exit 1
fi
if ! command -v sudo >/dev/null 2>&1; then
    echo "error: sudo is required to install xlurm system-wide" >&2
    exit 1
fi

cd -- "$SCRIPT_DIR"
cargo build --release --locked

artifacts=()
for binary in "${BINARIES[@]}"; do
    artifacts+=("$SCRIPT_DIR/target/release/$binary")
done

sudo install -o root -g root -m 0755 "${artifacts[@]}" "$INSTALL_DIR/"
echo "Installed ${BINARIES[*]} to $INSTALL_DIR"
