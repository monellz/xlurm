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

stage_dir="$(mktemp -d /tmp/xlurm-install.XXXXXX)"
trap 'rm -rf -- "$stage_dir"' EXIT
cp -- "${artifacts[@]}" "$stage_dir/"

staged_artifacts=()
for binary in "${BINARIES[@]}"; do
    staged_artifacts+=("$stage_dir/$binary")
done

sudo install -o root -g root -m 0755 "${staged_artifacts[@]}" "$INSTALL_DIR/"
echo "Installed ${BINARIES[*]} to $INSTALL_DIR"
