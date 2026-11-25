# Patches to cranelift-codegen 0.136.0

`0001` is the change Manuscript carried in `vendor/cranelift-codegen` before the compiler moved to C:

- x64 backend (`inst.isle`, `lower.isle`): a `brif` or `select` on the flag output of `sadd_overflow`, `ssub_overflow` or `usub_overflow` branches on the flags of the `add`/`sub` itself (`jo`, `jb`) instead of `seto`/`setb` plus a test. Upstream already does this for `uadd_overflow`, `umul_overflow` and `smul_overflow`. Checked `Int` arithmetic is one of these ops and a branch to a cold fault block, so each checked add or subtract loses two instructions.
- The fused and flags-unused rules take their second operand as `gpr_or_simm32` or `gpr_no_load`, fixing the `value_lowered_uses == 0` assertion in `sink_inst` that a producer lowered twice (once folded into the branch, once alone) could reach.
- `build.rs`: `VERSION` drops the `-<git revision>` suffix, so the build does not depend on the enclosing checkout.

Remove a patch when a Cranelift release contains the same change.
