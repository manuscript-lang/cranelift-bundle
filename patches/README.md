# Patches to Cranelift 0.136.0

One directory per crate (`apply-patches.sh` vendors each and applies its `*.patch` files).

## cranelift-object

`0001-coff-arm64-page-relocations`: the COFF writer had no mapping for `Aarch64AdrPrelPgHi21` and `Aarch64AddAbsLo12Nc`, which non-PIC aarch64 code uses for near symbols, so a Windows arm64 build panicked. They map to `IMAGE_REL_ARM64_PAGEBASE_REL21` and `IMAGE_REL_ARM64_PAGEOFFSET_12A`. Drop it when upstream has the same mapping.

## cranelift-codegen

`0001-x64-flag-fusion-and-stable-version` is the change Manuscript carried in `vendor/cranelift-codegen` before the compiler moved to C:

- x64 backend (`inst.isle`, `lower.isle`): a `brif` or `select` on the flag output of `sadd_overflow`, `ssub_overflow` or `usub_overflow` branches on the flags of the `add`/`sub` itself (`jo`, `jb`) instead of `seto`/`setb` plus a test. Upstream already does this for `uadd_overflow`, `umul_overflow` and `smul_overflow`. Checked `Int` arithmetic is one of these ops and a branch to a cold fault block, so each checked add or subtract loses two instructions.
- The fused and flags-unused rules take their second operand as `gpr_or_simm32` or `gpr_no_load`, fixing the `value_lowered_uses == 0` assertion in `sink_inst` that a producer lowered twice (once folded into the branch, once alone) could reach.
- `build.rs`: `VERSION` drops the `-<git revision>` suffix, so the build does not depend on the enclosing checkout.

Remove a patch when a Cranelift release contains the same change.
