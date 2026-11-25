# cranelift-bundle

A shared [Cranelift](https://cranelift.dev) code-generation library, one per platform (Linux, macOS and Windows on amd64 and arm64), behind a small C API (`include/cranelift.h`). A compiler builds functions and data through it, loading it by absolute path or linking it, and gets one relocatable object back. It is used by [Manuscript](https://github.com/manuscript-lang/manuscript) for fast Debug builds, but the API knows nothing about it.

`cranelift-version` pins the upstream crate. `patches/` holds our changes to it (see `patches/README.md`); `apply-patches.sh` copies the crate from the cargo registry and applies them. Each build includes only the instruction set of its own architecture, which keeps the library near 3 MB (Linux amd64).

## Releases

Push the tag `cranelift-<version>-r<revision>` (it must match `cranelift-version`); the build creates the release and attaches, per `<os>-<arch>`:

| Asset | Contents |
| --- | --- |
| `cranelift-lib-<version>-<os>-<arch>.tar.gz` | `lib/libcranelift.so` (Linux), `lib/libcranelift.dylib` (macOS) or `bin/cranelift.dll` (Windows), and `include/cranelift.h` |
| `SHA256SUMS` | the checksum of every asset |

Consumers verify a download against a checksum **pinned in their own repository**, never one fetched from the same release. Bump the revision when the recipe, the patches or the shim change without a version change.

## Using it

`include/cranelift.h` documents the whole API. It is generated: the function list, types and doc comments come from the `extern "C"` functions of `src/lib.rs`, the typedefs, enums and conveniences from `include/cranelift.h.in`; run `python3 gen-header.py` after changing either (CI runs it with `--check`). Handles are distinct struct types, every op has a name (`cl_iadd`, `cl_sqrt`, ...) or an enum for the generic call, and the same code links against the library or loads it at run time (`CL_DYNAMIC`, then `cl_bind` with `dlsym` or `GetProcAddress`):

```c
cl *c = cl_new(NULL, 0, NULL, NULL);                 // host target, default options
cl_sig s = cl_sig_new(c);
cl_sig_param(c, s, CL_I64, CL_EXT_NONE); cl_sig_param(c, s, CL_I64, CL_EXT_NONE);
cl_sig_ret(c, s, CL_I64, CL_EXT_NONE);
cl_func add = cl_declare_func(c, "add", s, CL_EXPORT);
cl_fn *f = cl_fn_new(c);                             // a builder: one per thread that builds
cl_fn_begin(f, add);
cl_block b = cl_entry(f);
cl_ret1(f, cl_iadd(f, cl_block_param(f, b, 0), cl_block_param(f, b, 1)));
cl_seal_all(f);
cl_fn_end(f);  /* queued; cl_wait or cl_finish reports errors */
size_t n; const uint8_t *object = cl_finish(c, &n);     // relocatable object, valid until cl_free
```

The surface covers i8 to i64 and f32/f64 values, arithmetic, checked arithmetic, conversions, compares, select, loads and stores (narrow and widening), atomics, stack slots, data with relocations, direct and indirect calls, branches, `switch`, traps and cold blocks. Settings (`opt_level`, `regalloc_algorithm=single_pass`, ...) pass through `cl_new`.

**Parallel compilation.** Declare every signature, function and data object first. `cl_fn_end` queues the function for worker threads inside the library (Rust threads; `threads` key, default one per core, 1 = compile on the calling thread), so the caller needs no threads of its own, and several builders on several threads work too. `cl_wait` blocks until the queue is empty; `cl_finish` writes the code in declaration order, so the object is the same however the workers interleave.

**Code places and debug info.** `cl_srcloc(f, file, line, column)` marks the instructions that follow; `cl_func_ranges` returns each function's code ranges as `{off, end, file, line, column}`, and `cl_func_info` the function's size and text offset, so a caller can build its own address-to-place tables. Data relocations may point into a function (`function + addend`) or into data, which is how such tables hold code addresses. When files are named (`cl_dwarf_file`), the object gets a DWARF 4 line table and a compile unit with a subprogram per function (names from `cl_func_name`); a row's column is the value given to `cl_srcloc`.

## Status

Variable and type DIEs are not written: only line tables. `smoke/` builds functions on four threads through the loaded library, links and runs them, and checks the relocations and the line table, on every platform in CI.

```sh
sh build.sh linux amd64   # needs cargo and patch; writes dist/
cc smoke/smoke.c -ldl -o smoke-test && ./smoke-test target/release/libcranelift.so smoke.o
cc smoke/main.c smoke.o -o smoke-run && ./smoke-run
```
