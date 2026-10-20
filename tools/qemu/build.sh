#!/usr/bin/env bash
# Build Espressif's QEMU with the esp-hal compatibility patch.
#
# Stock Espressif QEMU runs ESP-IDF firmware but not esp-hal/esp-rtos firmware
# (see README.md in this directory). This script clones the pinned release,
# applies the patch and installs qemu-system-xtensa into $PREFIX.
#
# Build dependencies (Debian/Ubuntu):
#   apt-get install -y git build-essential ninja-build python3-venv pkg-config \
#       libglib2.0-dev libpixman-1-dev libslirp-dev libgcrypt20-dev
#
# Usage: tools/qemu/build.sh [PREFIX]   (default: ~/.local/share/esp-qemu/patched)
set -euo pipefail

TAG=esp-develop-9.2.2-20260417
HERE="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${1:-$HOME/.local/share/esp-qemu/patched}"
WORK="${QEMU_BUILD_DIR:-$HOME/.cache/esp-qemu-build}"

if [ ! -d "$WORK/src" ]; then
    git clone --depth 1 --branch "$TAG" https://github.com/espressif/qemu.git "$WORK/src"
fi
cd "$WORK/src"
git checkout -q -- .
git apply "$HERE"/*.patch

mkdir -p build && cd build
../configure --prefix="$PREFIX" --target-list=xtensa-softmmu \
    --enable-gcrypt --enable-slirp \
    --disable-user --disable-capstone --disable-vnc --disable-sdl --disable-gtk \
    --disable-docs --disable-werror --disable-strip
ninja qemu-system-xtensa
ninja install >/dev/null
echo "installed $PREFIX/bin/qemu-system-xtensa"
