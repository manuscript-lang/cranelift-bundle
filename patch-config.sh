#!/bin/sh
# The cargo --config arguments that point each patched crate at vendor/: `cargo build $(sh patch-config.sh) ...`.
cd "$(dirname "$0")"
for dir in patches/*/; do
  crate=$(basename "$dir")
  printf '%s\n' "--config" "patch.crates-io.$crate.path=\"vendor/$crate\""
done
