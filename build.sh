#!/bin/sh
# Usage: sh build.sh <os> <arch>   (os: linux darwin windows; arch: amd64 arm64)
# Builds the shim with the instruction set of the platform's own architecture and packs dist/cranelift-lib-<version>-<os>-<arch>.tar.gz plus its .sha256.
set -eu
os=$1 arch=$2
version=$(tr -d '[:space:]' < "$(dirname "$0")/cranelift-version")
case "$arch" in amd64) feat=x86 ;; arm64) feat=arm64 ;; *) echo "bad arch $arch" >&2; exit 1 ;; esac
case "$os" in linux) lib=libcranelift.so ;; darwin) lib=libcranelift.dylib ;; windows) lib=cranelift.dll ;; *) echo "bad os $os" >&2; exit 1 ;; esac
cd "$(dirname "$0")"
sh apply-patches.sh
# shellcheck disable=SC2046
cargo build --release --locked $(sh patch-config.sh) --no-default-features --features "$feat"
out=dist/stage/cranelift-lib
rm -rf dist/stage && mkdir -p "$out/lib" "$out/include" dist
if [ "$os" = windows ]; then mkdir -p "$out/bin"; cp "target/release/$lib" "$out/bin/"; else cp "target/release/$lib" "$out/lib/"; fi
cp include/cranelift.h "$out/include/"
name="cranelift-lib-$version-$os-$arch.tar.gz"
tar -C dist/stage -czf "dist/$name" cranelift-lib
rm -rf dist/stage
if command -v sha256sum >/dev/null 2>&1; then (cd dist && sha256sum "$name" > "$name.sha256"); else (cd dist && shasum -a 256 "$name" > "$name.sha256"); fi
echo "sizes"; ls -l "target/release/$lib" | awk '{print "library", $5}'; ls -l "dist/$name" | awk '{print "archive", $5}'
