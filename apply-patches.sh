#!/bin/sh
# Copies the pinned cranelift-codegen from the cargo registry to vendor/ and applies patches/*.patch to it.
set -eu
cd "$(dirname "$0")"
version=$(tr -d '[:space:]' < cranelift-version)
# fetch rewrites Cargo.lock without the patch entry, so keep the committed one
cp Cargo.lock Cargo.lock.keep
cargo fetch >/dev/null
mv Cargo.lock.keep Cargo.lock
src=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/cranelift-codegen-"$version" | head -n 1)
rm -rf vendor/cranelift-codegen && mkdir -p vendor
cp -R "$src" vendor/cranelift-codegen
chmod -R u+w vendor/cranelift-codegen
for p in patches/*.patch; do patch -d vendor/cranelift-codegen -p1 < "$p"; done
