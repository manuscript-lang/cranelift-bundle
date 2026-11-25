#!/bin/sh
# For each patches/<crate>/ directory: copies the pinned <crate> from the cargo registry to vendor/<crate> and applies
# its *.patch files. build.sh then builds with those copies in place of the registry crates (see patch-config.sh).
set -eu
cd "$(dirname "$0")"
version=$(tr -d '[:space:]' < cranelift-version)
# fetch rewrites Cargo.lock without the patch entries, so keep the committed one
cp Cargo.lock Cargo.lock.keep
cargo fetch >/dev/null
mv Cargo.lock.keep Cargo.lock
rm -rf vendor && mkdir vendor
for dir in patches/*/; do
  crate=$(basename "$dir")
  src=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/"$crate"-"$version" | head -n 1)
  cp -R "$src" "vendor/$crate"
  chmod -R u+w "vendor/$crate"
  for p in "$dir"*.patch; do patch -d "vendor/$crate" -p1 < "$p"; done
done
