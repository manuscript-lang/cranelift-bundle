# cranelift-bundle

A shared [Cranelift](https://cranelift.dev) code-generation library for [Manuscript](https://github.com/manuscript-lang/manuscript), one per platform (Linux, macOS and Windows on amd64 and arm64), behind a small C API (`include/ms_cranelift.h`). Manuscript's C compiler loads it by absolute path, the same way it loads the bundled LLVM library from [llvm-bundle](https://github.com/manuscript-lang/llvm-bundle), and lowers MIR through it for fast Debug builds.

`cranelift-version` pins the upstream crate. `patches/` holds our changes to it (see `patches/README.md`); `apply-patches.sh` copies the crate from the cargo registry and applies them. Each build includes only the instruction set of its own architecture, which keeps the library near 3 MB (Linux amd64).

## Releases

Push the tag `cranelift-<version>-r<revision>` (it must match `cranelift-version`); the build creates the release and attaches, per `<os>-<arch>`:

| Asset | Contents |
| --- | --- |
| `cranelift-lib-<version>-<os>-<arch>.tar.gz` | `lib/libms_cranelift.so` (Linux), `lib/libms_cranelift.dylib` (macOS) or `bin/ms_cranelift.dll` (Windows), and `include/ms_cranelift.h` |
| `SHA256SUMS` | the checksum of every asset |

Consumers verify a download against a checksum **pinned in their own repository**, never one fetched from the same release. Bump the revision when the recipe, the patches or the shim change without a version change.

## Using it

`include/ms_cranelift.h` documents the whole API. Handles are distinct struct types, every op has a name (`ms_cl_iadd`, `ms_cl_sqrt`, ...) or an enum for the generic call, and the same code links against the library or loads it at run time (`MS_CL_DYNAMIC`, then `ms_cl_bind` with `dlsym` or `GetProcAddress`):

```c
ms_cl *c = ms_cl_new(NULL, 0, NULL, NULL);                 // host target, default options
ms_cl_sig s = ms_cl_sig_new(c);
ms_cl_sig_param(c, s, MS_CL_I64, MS_CL_EXT_NONE); ms_cl_sig_param(c, s, MS_CL_I64, MS_CL_EXT_NONE);
ms_cl_sig_ret(c, s, MS_CL_I64, MS_CL_EXT_NONE);
ms_cl_func add = ms_cl_declare_func(c, "add", s, MS_CL_EXPORT);
ms_cl_fn *f = ms_cl_fn_new(c);                             // a builder: one per thread that builds
ms_cl_fn_begin(f, add);
ms_cl_block b = ms_cl_entry(f);
ms_cl_ret1(f, ms_cl_iadd(f, ms_cl_block_param(f, b, 0), ms_cl_block_param(f, b, 1)));
ms_cl_seal_all(f);
if (ms_cl_fn_end(f) || ms_cl_define(c, f)) fprintf(stderr, "%s\n", ms_cl_error(c));
size_t n; const uint8_t *object = ms_cl_finish(c, &n);     // relocatable object, valid until ms_cl_free
```

The surface covers i8 to i64 and f32/f64 values, arithmetic, checked arithmetic, conversions, compares, select, loads and stores (narrow and widening), atomics, stack slots, data with relocations, direct and indirect calls, branches, `switch`, traps and cold blocks. Settings (`opt_level`, `regalloc_algorithm=single_pass`, ...) pass through `ms_cl_new`.

**Parallel compilation.** Declare every signature, function and data object first. Then each thread owns a builder (`ms_cl_fn`), builds and compiles functions (`ms_cl_fn_end`) without touching shared state, and hands each result to the module with `ms_cl_define`, from any thread in any order. `ms_cl_finish` writes the code in declaration order, so the object is the same however the threads interleave.

**Fault tables and debug info.** `ms_cl_srcloc(f, file, line, scope)` marks the instructions that follow; `ms_cl_func_ranges` returns each function's code ranges as `{off, end, file, line, scope}`, the input of the tables Manuscript hands `msrt_start`, and `ms_cl_func_info` the function's size and text offset. Data relocations may point into a function (`function + addend`) or into data, which is how those tables hold code addresses. When files are named (`ms_cl_dwarf_file`), the object gets a DWARF 4 line table and a compile unit with a subprogram per function (names from `ms_cl_func_name`); a row's column is the scope number, as in the LLVM route's `.loc`.

## Status

The API is complete for what the old Manuscript Cranelift lowering used (checked against its source); the MIR lowering that calls it lives in manuscript. Variable and type DIEs (the old `dwarf.rs` emitted them) are not written: main emits line tables only. `smoke/` builds functions on four threads through the loaded library, links and runs them, and checks the relocations and the line table, on every platform in CI.

```sh
sh build.sh linux amd64   # needs cargo and patch; writes dist/
cc smoke/smoke.c -ldl -o smoke-test && ./smoke-test target/release/libms_cranelift.so smoke.o
cc smoke/main.c smoke.o -o smoke-run && ./smoke-run
```
