# cranelift-bundle

A shared [Cranelift](https://cranelift.dev) code-generation library for [Manuscript](https://github.com/manuscript-lang/manuscript), one per platform (Linux, macOS and Windows on amd64 and arm64), behind a small C API (`include/ms_cranelift.h`). Manuscript's C compiler loads it by absolute path, the same way it loads the bundled LLVM library from [llvm-bundle](https://github.com/manuscript-lang/llvm-bundle), and lowers MIR through it for fast Debug builds.

`cranelift-version` pins the upstream crate. `patches/` holds our changes to it (see `patches/README.md`); `apply-patches.sh` copies the crate from the cargo registry and applies them. Each build includes only the instruction set of its own architecture, which keeps the library near 3 MB (Linux amd64).

## Releases

Create the release `cranelift-<version>-r<revision>` for a tag by hand; the build attaches, per `<os>-<arch>`:

| Asset | Contents |
| --- | --- |
| `cranelift-lib-<version>-<os>-<arch>.tar.gz` | `lib/libms_cranelift.so` (Linux), `lib/libms_cranelift.dylib` (macOS) or `bin/ms_cranelift.dll` (Windows), and `include/ms_cranelift.h` |
| `SHA256SUMS` | the checksum of every asset |

Consumers verify a download against a checksum **pinned in their own repository**, never one fetched from the same release. Bump the revision when the recipe, the patches or the shim change without a version change.

## Status

The shim is a spike: i64 values, integer ops, checked arithmetic, compare, branch, call and return. Floats, narrow integers, memory, stack slots, multi-value returns, data and relocations, and line tables are still to come. Windows and macOS builds have not run yet.

```sh
sh build.sh linux amd64   # needs cargo and patch; writes dist/
cc smoke/smoke.c -ldl -o smoke && ./smoke target/release/libms_cranelift.so
```
