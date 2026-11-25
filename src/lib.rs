//! A C ABI over Cranelift: everything the Manuscript MIR lowering needs to build functions and data,
//! and write one relocatable object. Handles are u32 indices; values, blocks and stack slots are per
//! function, signatures, functions and data are per module. See `include/ms_cranelift.h` for the
//! numbering of the op codes, which this file must match.
//!
//! The functions never unwind across the boundary (`panic = "abort"`). A bad handle aborts the process:
//! the caller is a compiler, not an untrusted client. Failures that depend on the input (the verifier,
//! an unknown flag) are reported through `ms_cl_error`.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    self, types, AbiParam, AtomicRmwOp, Block, BlockArg, FuncRef, GlobalValue, InstBuilder, MemFlagsData,
    SigRef, Signature, SourceLoc, StackSlot, StackSlotData, StackSlotKind, TrapCode, Type, UserFuncName, Value,
};
use cranelift_codegen::isa::{self, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
#[allow(unused_imports)]
use cranelift_module::FuncOrDataId;
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::str::FromStr;

macro_rules! handle {
    ($($n:ident),*) => {$(
        /// A u32 index in a wrapper the C compiler can tell from the other handle types.
        #[repr(transparent)]
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub struct $n(u32);
    )*};
}
handle!(Val, BlockId, SlotId, SigId, Func, Data);

/// A checked arithmetic result and its overflow flag (an i8).
#[repr(C)]
pub struct Ovf {
    pub result: Val,
    pub flag: Val,
}

/// A source-location range of one function, as machine-code offsets from the function's start.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClSrcLoc {
    pub start: u32,
    pub end: u32,
    pub loc: u32,
}

/// A pointer-sized relocation in a data object: at `offset` store the address of function or data
/// `target` plus `addend`. `kind` 0 is a function, 1 is data.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClReloc {
    pub offset: u32,
    pub kind: u32,
    pub target: u32,
    pub addend: i64,
}

#[derive(Default)]
struct FuncInfo {
    srclocs: Vec<ClSrcLoc>,
    size: u32,
    /// Offset in the object's text section, set by `finish`.
    offset: u64,
}

pub struct Cl {
    module: Option<ObjectModule>,
    ctx: Context,
    fctx: FunctionBuilderContext,
    sigs: Vec<Signature>,
    funcs: Vec<FuncId>,
    func_sigs: Vec<u32>,
    datas: Vec<DataId>,
    info: Vec<FuncInfo>,
    // Per function state.
    builder: Option<FunctionBuilder<'static>>,
    cur: u32,
    blocks: Vec<Block>,
    vals: Vec<Value>,
    slots: Vec<StackSlot>,
    frefs: HashMap<u32, FuncRef>,
    grefs: HashMap<u32, GlobalValue>,
    srefs: HashMap<u32, SigRef>,
    // Results.
    object: Vec<u8>,
    err: CString,
    dump: CString,
}

fn cl<'a>(c: *mut Cl) -> &'a mut Cl {
    unsafe { &mut *c }
}

fn bld(c: &mut Cl) -> &mut FunctionBuilder<'static> {
    c.builder.as_mut().expect("ms_cl_func_begin was not called")
}

fn module(c: &mut Cl) -> &mut ObjectModule {
    c.module.as_mut().expect("the module was finished")
}

fn cstr<'a>(p: *const c_char) -> &'a str {
    unsafe { CStr::from_ptr(p) }.to_str().expect("a name is UTF-8")
}

fn set_err(c: &mut Cl, msg: impl std::fmt::Display) {
    c.err = CString::new(msg.to_string().replace('\0', " ")).unwrap();
}

fn ty(t: u32) -> Type {
    match t {
        0 => types::I8,
        1 => types::I16,
        2 => types::I32,
        3 => types::I64,
        4 => types::F32,
        5 => types::F64,
        _ => panic!("bad type code {t}"),
    }
}

fn abi(t: u32, ext: u32) -> AbiParam {
    let p = AbiParam::new(ty(t));
    match ext {
        1 => p.uext(),
        2 => p.sext(),
        _ => p,
    }
}

fn mflags(f: u32) -> MemFlagsData {
    let mut m = MemFlagsData::new();
    if f & 1 != 0 {
        m = m.with_notrap();
    }
    if f & 2 != 0 {
        m = m.with_aligned();
    }
    if f & 4 != 0 {
        m = m.with_readonly();
    }
    if f & 8 != 0 {
        m = m.with_can_move();
    }
    m
}

fn push(c: &mut Cl, v: Value) -> Val {
    c.vals.push(v);
    Val((c.vals.len() - 1) as u32)
}

fn val(c: &Cl, v: Val) -> Value {
    c.vals[v.0 as usize]
}

unsafe fn list<T: Copy>(p: *const T, n: u32) -> Vec<T> {
    if n == 0 { Vec::new() } else { std::slice::from_raw_parts(p, n as usize).to_vec() }
}

fn block_args(c: &Cl, a: *const Val, n: u32) -> Vec<BlockArg> {
    unsafe { list(a, n) }.into_iter().map(|v| BlockArg::Value(val(c, v))).collect()
}

fn values(c: &Cl, a: *const Val, n: u32) -> Vec<Value> {
    unsafe { list(a, n) }.into_iter().map(|v| val(c, v)).collect()
}

// ---- Module ----------------------------------------------------------------------------------

/// Creates a module for `triple` (null: the host). `keys[i]`/`vals[i]` are Cranelift settings that
/// override the defaults (for example `opt_level` = `none`, `regalloc_algorithm` = `single_pass`).
/// Returns null and sets the error (read with `ms_cl_error(null)`) when the target or a setting is unknown.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_new(
    triple: *const c_char,
    n: u32,
    keys: *const *const c_char,
    vals: *const *const c_char,
) -> *mut Cl {
    let r = (|| -> Result<Cl, String> {
        let triple = if triple.is_null() {
            target_lexicon::Triple::host()
        } else {
            target_lexicon::Triple::from_str(cstr(triple)).map_err(|e| e.to_string())?
        };
        let arm64_coff = triple.architecture == target_lexicon::Architecture::Aarch64(target_lexicon::Aarch64Architecture::Aarch64)
            && triple.operating_system == target_lexicon::OperatingSystem::Windows;
        let mut flags = settings::builder();
        let mut set = |k: &str, v: &str| flags.set(k, v).map_err(|e| format!("setting {k}={v}: {e}"));
        set("opt_level", "none")?;
        set("is_pic", if arm64_coff { "false" } else { "true" })?;
        set("unwind_info", "false")?;
        set("enable_verifier", "false")?;
        set("preserve_frame_pointers", "true")?;
        set("enable_multi_ret_implicit_sret", "true")?;
        set("enable_probestack", "true")?;
        set("probestack_strategy", "inline")?;
        for i in 0..n as usize {
            set(cstr(*keys.add(i)), cstr(*vals.add(i)))?;
        }
        let isa: OwnedTargetIsa = isa::lookup(triple)
            .map_err(|e| e.to_string())?
            .finish(settings::Flags::new(flags))
            .map_err(|e| e.to_string())?;
        let ob = ObjectBuilder::new(isa, "ms", cranelift_module::default_libcall_names()).map_err(|e| e.to_string())?;
        Ok(Cl {
            module: Some(ObjectModule::new(ob)),
            ctx: Context::new(),
            fctx: FunctionBuilderContext::new(),
            sigs: vec![],
            funcs: vec![],
            func_sigs: vec![],
            datas: vec![],
            info: vec![],
            builder: None,
            cur: 0,
            blocks: vec![],
            vals: vec![],
            slots: vec![],
            frefs: HashMap::new(),
            grefs: HashMap::new(),
            srefs: HashMap::new(),
            object: vec![],
            err: CString::default(),
            dump: CString::default(),
        })
    })();
    match r {
        Ok(c) => Box::into_raw(Box::new(c)),
        Err(e) => {
            LAST_ERROR.with(|l| *l.borrow_mut() = CString::new(e.replace('\0', " ")).unwrap());
            std::ptr::null_mut()
        }
    }
}

thread_local! {
    static LAST_ERROR: std::cell::RefCell<CString> = std::cell::RefCell::new(CString::default());
}

/// The last error of `c`, or of `ms_cl_new` when `c` is null. Valid until the next call on `c`.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_error(c: *mut Cl) -> *const c_char {
    if c.is_null() {
        LAST_ERROR.with(|l| l.borrow().as_ptr())
    } else {
        cl(c).err.as_ptr()
    }
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_free(c: *mut Cl) {
    if !c.is_null() {
        // The builder borrows `ctx` and `fctx`: drop it first.
        let mut c = Box::from_raw(c);
        c.builder = None;
    }
}

/// The calling convention name of the target's C ABI is used for every signature; `ms_cl_sig` starts a signature.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_sig_new(c: *mut Cl) -> SigId {
    let c = cl(c);
    let cc = module(c).isa().default_call_conv();
    c.sigs.push(Signature::new(cc));
    SigId((c.sigs.len() - 1) as u32)
}

/// Adds a parameter (`ext` 0 none, 1 zero-extend, 2 sign-extend to a full register).
#[no_mangle]
pub unsafe extern "C" fn ms_cl_sig_param(c: *mut Cl, sig: SigId, t: u32, ext: u32) {
    cl(c).sigs[sig.0 as usize].params.push(abi(t, ext));
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_sig_ret(c: *mut Cl, sig: SigId, t: u32, ext: u32) {
    cl(c).sigs[sig.0 as usize].returns.push(abi(t, ext));
}

fn linkage(l: u32) -> Linkage {
    match l {
        0 => Linkage::Import,
        1 => Linkage::Export,
        2 => Linkage::Local,
        _ => Linkage::Preemptible,
    }
}

/// Declares a function; `link` 0 import, 1 export, 2 local (hidden), 3 preemptible.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_declare_func(c: *mut Cl, name: *const c_char, sig: SigId, link: u32) -> Func {
    let c = cl(c);
    let s = c.sigs[sig.0 as usize].clone();
    let id = module(c).declare_function(cstr(name), linkage(link), &s).expect("declare_function");
    c.funcs.push(id);
    c.func_sigs.push(sig.0);
    c.info.push(FuncInfo::default());
    Func((c.funcs.len() - 1) as u32)
}

/// Declares a data object (`tls` makes it thread-local).
#[no_mangle]
pub unsafe extern "C" fn ms_cl_declare_data(c: *mut Cl, name: *const c_char, link: u32, writable: i32, tls: i32) -> Data {
    let c = cl(c);
    let id = module(c).declare_data(cstr(name), linkage(link), writable != 0, tls != 0).expect("declare_data");
    c.datas.push(id);
    Data((c.datas.len() - 1) as u32)
}

/// Defines a data object of `len` bytes: `bytes` (null for zeros), alignment `align`, and `n` relocations.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_define_data(
    c: *mut Cl,
    data: Data,
    bytes: *const u8,
    len: u32,
    align: u32,
    relocs: *const ClReloc,
    n: u32,
) {
    let c = cl(c);
    let mut d = DataDescription::new();
    if bytes.is_null() && n == 0 {
        d.define_zeroinit(len as usize);
    } else if bytes.is_null() {
        // Relocations need initialized data: a zero-init object lives in .bss, which cannot hold them.
        d.define(vec![0u8; len as usize].into_boxed_slice());
    } else {
        d.define(std::slice::from_raw_parts(bytes, len as usize).to_vec().into_boxed_slice());
    }
    if align > 1 {
        d.set_align(align as u64);
    }
    for r in list(relocs, n) {
        if r.kind == 0 {
            let id = c.funcs[r.target as usize];
            let f = module(c).declare_func_in_data(id, &mut d);
            d.write_function_addr(r.offset, f);
        } else {
            let id = c.datas[r.target as usize];
            let g = module(c).declare_data_in_data(id, &mut d);
            d.write_data_addr(r.offset, g, r.addend);
        }
    }
    let id = c.datas[data.0 as usize];
    module(c).define_data(id, &d).expect("define_data");
}

// ---- Function bodies -------------------------------------------------------------------------

/// Starts the body of a declared function. Create its blocks next; `ms_cl_entry` makes the entry block.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_func_begin(c: *mut Cl, func: Func) {
    let c = cl(c);
    let m = c.module.as_mut().unwrap();
    m.clear_context(&mut c.ctx);
    c.ctx.func.signature = c.sigs[c.func_sigs[func.0 as usize] as usize].clone();
    c.ctx.func.name = UserFuncName::user(0, func.0);
    c.cur = func.0;
    c.blocks.clear();
    c.vals.clear();
    c.slots.clear();
    c.frefs.clear();
    c.grefs.clear();
    c.srefs.clear();
    // The builder borrows the context and the function for as long as the body is built.
    let f: &'static mut ir::Function = std::mem::transmute(&mut c.ctx.func);
    let fc: &'static mut FunctionBuilderContext = std::mem::transmute(&mut c.fctx);
    c.builder = Some(FunctionBuilder::new(f, fc));
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_block_new(c: *mut Cl) -> BlockId {
    let c = cl(c);
    let b = bld(c).create_block();
    c.blocks.push(b);
    BlockId((c.blocks.len() - 1) as u32)
}

/// Adds a parameter of type `t` to block `blk` and returns its value.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_block_param_add(c: *mut Cl, blk: BlockId, t: u32) -> Val {
    let c = cl(c);
    let b = c.blocks[blk.0 as usize];
    let v = bld(c).append_block_param(b, ty(t));
    push(c, v)
}

/// Parameter `i` of block `blk` (for the entry block, of the function).
#[no_mangle]
pub unsafe extern "C" fn ms_cl_block_param(c: *mut Cl, blk: BlockId, i: u32) -> Val {
    let c = cl(c);
    let b = c.blocks[blk.0 as usize];
    let v = bld(c).block_params(b)[i as usize];
    push(c, v)
}

/// Creates the entry block, whose parameters are the function's, and enters it.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_entry(c: *mut Cl) -> BlockId {
    let c = cl(c);
    let b = bld(c).create_block();
    bld(c).append_block_params_for_function_params(b);
    bld(c).switch_to_block(b);
    c.blocks.push(b);
    BlockId((c.blocks.len() - 1) as u32)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_enter(c: *mut Cl, blk: BlockId) {
    let c = cl(c);
    let b = c.blocks[blk.0 as usize];
    bld(c).switch_to_block(b);
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_seal(c: *mut Cl, blk: BlockId) {
    let c = cl(c);
    let b = c.blocks[blk.0 as usize];
    bld(c).seal_block(b);
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_seal_all(c: *mut Cl) {
    bld(cl(c)).seal_all_blocks();
}

/// Lays `blk` out of line: its code goes after the hot code.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_cold(c: *mut Cl, blk: BlockId) {
    let c = cl(c);
    let b = c.blocks[blk.0 as usize];
    bld(c).set_cold_block(b);
}

/// The source location (an opaque u32) of the instructions that follow.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_srcloc(c: *mut Cl, loc: u32) {
    bld(cl(c)).set_srcloc(SourceLoc::new(loc));
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_stack_slot(c: *mut Cl, size: u32, align_shift: u8) -> SlotId {
    let c = cl(c);
    let s = bld(c).create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, align_shift));
    c.slots.push(s);
    SlotId((c.slots.len() - 1) as u32)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_stack_addr(c: *mut Cl, slot: SlotId, off: i32) -> Val {
    let c = cl(c);
    let s = c.slots[slot.0 as usize];
    let v = bld(c).ins().stack_addr(types::I64, s, off);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_sp(c: *mut Cl) -> Val {
    let c = cl(c);
    let v = bld(c).ins().get_stack_pointer(types::I64);
    push(c, v)
}

// ---- Constants and arithmetic ----------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn ms_cl_iconst(c: *mut Cl, t: u32, k: i64) -> Val {
    let c = cl(c);
    let v = bld(c).ins().iconst(ty(t), k);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_f32const(c: *mut Cl, k: f32) -> Val {
    let c = cl(c);
    let v = bld(c).ins().f32const(k);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_f64const(c: *mut Cl, k: f64) -> Val {
    let c = cl(c);
    let v = bld(c).ins().f64const(k);
    push(c, v)
}

/// 0 ineg 1 bnot 2 clz 3 ctz 4 popcnt 5 bswap 6 bitrev 7 iabs 8 fneg 9 fabs 10 sqrt 11 ceil 12 floor 13 trunc 14 nearest.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_unary(c: *mut Cl, op: u32, x: Val) -> Val {
    let c = cl(c);
    let x = val(c, x);
    let i = bld(c).ins();
    let v = match op {
        0 => i.ineg(x),
        1 => i.bnot(x),
        2 => i.clz(x),
        3 => i.ctz(x),
        4 => i.popcnt(x),
        5 => i.bswap(x),
        6 => i.bitrev(x),
        7 => i.iabs(x),
        8 => i.fneg(x),
        9 => i.fabs(x),
        10 => i.sqrt(x),
        11 => i.ceil(x),
        12 => i.floor(x),
        13 => i.trunc(x),
        14 => i.nearest(x),
        _ => panic!("bad unary op {op}"),
    };
    push(c, v)
}

/// 0 iadd 1 isub 2 imul 3 sdiv 4 udiv 5 srem 6 urem 7 band 8 bor 9 bxor 10 ishl 11 sshr 12 ushr 13 rotl 14 rotr
/// 15 umulhi 16 smulhi 17 fadd 18 fsub 19 fmul 20 fdiv 21 fmin 22 fmax 23 fcopysign 24 smin 25 smax 26 umin 27 umax
/// 28 band_not 29 bor_not 30 bxor_not.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_binary(c: *mut Cl, op: u32, x: Val, y: Val) -> Val {
    let c = cl(c);
    let (x, y) = (val(c, x), val(c, y));
    let i = bld(c).ins();
    let v = match op {
        0 => i.iadd(x, y),
        1 => i.isub(x, y),
        2 => i.imul(x, y),
        3 => i.sdiv(x, y),
        4 => i.udiv(x, y),
        5 => i.srem(x, y),
        6 => i.urem(x, y),
        7 => i.band(x, y),
        8 => i.bor(x, y),
        9 => i.bxor(x, y),
        10 => i.ishl(x, y),
        11 => i.sshr(x, y),
        12 => i.ushr(x, y),
        13 => i.rotl(x, y),
        14 => i.rotr(x, y),
        15 => i.umulhi(x, y),
        16 => i.smulhi(x, y),
        17 => i.fadd(x, y),
        18 => i.fsub(x, y),
        19 => i.fmul(x, y),
        20 => i.fdiv(x, y),
        21 => i.fmin(x, y),
        22 => i.fmax(x, y),
        23 => i.fcopysign(x, y),
        24 => i.smin(x, y),
        25 => i.smax(x, y),
        26 => i.umin(x, y),
        27 => i.umax(x, y),
        28 => i.band_not(x, y),
        29 => i.bor_not(x, y),
        30 => i.bxor_not(x, y),
        _ => panic!("bad binary op {op}"),
    };
    push(c, v)
}

/// Overflow-checked arithmetic: 0 sadd 1 uadd 2 ssub 3 usub 4 smul 5 umul. Returns the result; the overflow
/// flag (an i8) is the value after it.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_ovf(c: *mut Cl, op: u32, x: Val, y: Val) -> Ovf {
    let c = cl(c);
    let (x, y) = (val(c, x), val(c, y));
    let i = bld(c).ins();
    let (v, f) = match op {
        0 => i.sadd_overflow(x, y),
        1 => i.uadd_overflow(x, y),
        2 => i.ssub_overflow(x, y),
        3 => i.usub_overflow(x, y),
        4 => i.smul_overflow(x, y),
        5 => i.umul_overflow(x, y),
        _ => panic!("bad overflow op {op}"),
    };
    Ovf { result: push(c, v), flag: push(c, f) }
}

/// Conversions to type `t`: 0 uextend 1 sextend 2 ireduce 3 fpromote 4 fdemote 5 fcvt_from_sint 6 fcvt_from_uint
/// 7 fcvt_to_sint 8 fcvt_to_uint 9 fcvt_to_sint_sat 10 fcvt_to_uint_sat 11 bitcast.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_cast(c: *mut Cl, op: u32, t: u32, x: Val) -> Val {
    let c = cl(c);
    let (t, x) = (ty(t), val(c, x));
    let i = bld(c).ins();
    let v = match op {
        0 => i.uextend(t, x),
        1 => i.sextend(t, x),
        2 => i.ireduce(t, x),
        3 => i.fpromote(t, x),
        4 => i.fdemote(t, x),
        5 => i.fcvt_from_sint(t, x),
        6 => i.fcvt_from_uint(t, x),
        7 => i.fcvt_to_sint(t, x),
        8 => i.fcvt_to_uint(t, x),
        9 => i.fcvt_to_sint_sat(t, x),
        10 => i.fcvt_to_uint_sat(t, x),
        11 => i.bitcast(t, ir::MemFlagsData::new(), x),
        _ => panic!("bad cast op {op}"),
    };
    push(c, v)
}

/// Integer compare (an i8): 0 eq 1 ne 2 slt 3 sge 4 sgt 5 sle 6 ult 7 uge 8 ugt 9 ule.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_icmp(c: *mut Cl, cc: u32, x: Val, y: Val) -> Val {
    let c = cl(c);
    let cc = match cc {
        0 => IntCC::Equal,
        1 => IntCC::NotEqual,
        2 => IntCC::SignedLessThan,
        3 => IntCC::SignedGreaterThanOrEqual,
        4 => IntCC::SignedGreaterThan,
        5 => IntCC::SignedLessThanOrEqual,
        6 => IntCC::UnsignedLessThan,
        7 => IntCC::UnsignedGreaterThanOrEqual,
        8 => IntCC::UnsignedGreaterThan,
        9 => IntCC::UnsignedLessThanOrEqual,
        _ => panic!("bad icmp code {cc}"),
    };
    let (x, y) = (val(c, x), val(c, y));
    let v = bld(c).ins().icmp(cc, x, y);
    push(c, v)
}

/// Float compare (an i8): 0 eq 1 ne 2 lt 3 le 4 gt 5 ge 6 ord 7 uno 8 ueq 9 one 10 ult 11 ule 12 ugt 13 uge.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_fcmp(c: *mut Cl, cc: u32, x: Val, y: Val) -> Val {
    let c = cl(c);
    let cc = match cc {
        0 => FloatCC::Equal,
        1 => FloatCC::NotEqual,
        2 => FloatCC::LessThan,
        3 => FloatCC::LessThanOrEqual,
        4 => FloatCC::GreaterThan,
        5 => FloatCC::GreaterThanOrEqual,
        6 => FloatCC::Ordered,
        7 => FloatCC::Unordered,
        8 => FloatCC::UnorderedOrEqual,
        9 => FloatCC::OrderedNotEqual,
        10 => FloatCC::UnorderedOrLessThan,
        11 => FloatCC::UnorderedOrLessThanOrEqual,
        12 => FloatCC::UnorderedOrGreaterThan,
        13 => FloatCC::UnorderedOrGreaterThanOrEqual,
        _ => panic!("bad fcmp code {cc}"),
    };
    let (x, y) = (val(c, x), val(c, y));
    let v = bld(c).ins().fcmp(cc, x, y);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_select(c: *mut Cl, cond: Val, a: Val, b: Val) -> Val {
    let c = cl(c);
    let (cond, a, b) = (val(c, cond), val(c, a), val(c, b));
    let v = bld(c).ins().select(cond, a, b);
    push(c, v)
}

// ---- Memory ----------------------------------------------------------------------------------

/// Memory flags: 1 notrap 2 aligned 4 readonly 8 can_move.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_load(c: *mut Cl, t: u32, flags: u32, base: Val, off: i32) -> Val {
    let c = cl(c);
    let base = val(c, base);
    let v = bld(c).ins().load(ty(t), mflags(flags), base, off);
    push(c, v)
}

/// A load of `bits` (8, 16 or 32) widened to type `t`: `signed` selects sign extension.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_load_ext(c: *mut Cl, bits: u32, signed: i32, t: u32, flags: u32, base: Val, off: i32) -> Val {
    let c = cl(c);
    let (t, f, base) = (ty(t), mflags(flags), val(c, base));
    let i = bld(c).ins();
    let v = match (bits, signed != 0) {
        (8, false) => i.uload8(t, f, base, off),
        (16, false) => i.uload16(t, f, base, off),
        (32, false) => i.uload32(f, base, off),
        (8, true) => i.sload8(t, f, base, off),
        (16, true) => i.sload16(t, f, base, off),
        (32, true) => i.sload32(f, base, off),
        _ => panic!("bad load width {bits}"),
    };
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_store(c: *mut Cl, flags: u32, v: Val, base: Val, off: i32) {
    let c = cl(c);
    let (v, base) = (val(c, v), val(c, base));
    bld(c).ins().store(mflags(flags), v, base, off);
}

/// A store of the low `bits` (8, 16 or 32) of an integer value.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_store_trunc(c: *mut Cl, bits: u32, flags: u32, v: Val, base: Val, off: i32) {
    let c = cl(c);
    let (f, v, base) = (mflags(flags), val(c, v), val(c, base));
    let i = bld(c).ins();
    match bits {
        8 => i.istore8(f, v, base, off),
        16 => i.istore16(f, v, base, off),
        32 => i.istore32(f, v, base, off),
        _ => panic!("bad store width {bits}"),
    };
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_atomic_load(c: *mut Cl, t: u32, flags: u32, addr: Val) -> Val {
    let c = cl(c);
    let addr = val(c, addr);
    let v = bld(c).ins().atomic_load(ty(t), mflags(flags), addr);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_atomic_store(c: *mut Cl, flags: u32, v: Val, addr: Val) {
    let c = cl(c);
    let (v, addr) = (val(c, v), val(c, addr));
    bld(c).ins().atomic_store(mflags(flags), v, addr);
}

/// 0 add 1 sub 2 and 3 nand 4 or 5 xor 6 xchg 7 umin 8 umax 9 smin 10 smax. Returns the old value.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_atomic_rmw(c: *mut Cl, op: u32, t: u32, flags: u32, addr: Val, x: Val) -> Val {
    let c = cl(c);
    let op = match op {
        0 => AtomicRmwOp::Add,
        1 => AtomicRmwOp::Sub,
        2 => AtomicRmwOp::And,
        3 => AtomicRmwOp::Nand,
        4 => AtomicRmwOp::Or,
        5 => AtomicRmwOp::Xor,
        6 => AtomicRmwOp::Xchg,
        7 => AtomicRmwOp::Umin,
        8 => AtomicRmwOp::Umax,
        9 => AtomicRmwOp::Smin,
        10 => AtomicRmwOp::Smax,
        _ => panic!("bad atomic op {op}"),
    };
    let (addr, x) = (val(c, addr), val(c, x));
    let v = bld(c).ins().atomic_rmw(ty(t), mflags(flags), op, addr, x);
    push(c, v)
}

/// Compare-and-swap; returns the old value.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_atomic_cas(c: *mut Cl, flags: u32, addr: Val, expected: Val, new: Val) -> Val {
    let c = cl(c);
    let (addr, e, n) = (val(c, addr), val(c, expected), val(c, new));
    let v = bld(c).ins().atomic_cas(mflags(flags), addr, e, n);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_fence(c: *mut Cl) {
    bld(cl(c)).ins().fence();
}

// ---- Addresses, calls, control flow ----------------------------------------------------------

fn fref(c: &mut Cl, func: Func) -> FuncRef {
    if let Some(r) = c.frefs.get(&func.0) {
        return *r;
    }
    let id = c.funcs[func.0 as usize];
    let m = c.module.as_mut().unwrap();
    let r = m.declare_func_in_func(id, &mut c.builder.as_mut().unwrap().func);
    c.frefs.insert(func.0, r);
    r
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_func_addr(c: *mut Cl, func: Func) -> Val {
    let c = cl(c);
    let r = fref(c, func);
    let v = bld(c).ins().func_addr(types::I64, r);
    push(c, v)
}

/// The address of a data object (a symbol value).
#[no_mangle]
pub unsafe extern "C" fn ms_cl_data_addr(c: *mut Cl, data: Data) -> Val {
    let c = cl(c);
    let g = match c.grefs.get(&data.0) {
        Some(g) => *g,
        None => {
            let id = c.datas[data.0 as usize];
            let m = c.module.as_mut().unwrap();
            let g = m.declare_data_in_func(id, &mut c.builder.as_mut().unwrap().func);
            c.grefs.insert(data.0, g);
            g
        }
    };
    let v = bld(c).ins().symbol_value(types::I64, g);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_jump(c: *mut Cl, blk: BlockId, args: *const Val, n: u32) {
    let c = cl(c);
    let a = block_args(c, args, n);
    let b = c.blocks[blk.0 as usize];
    bld(c).ins().jump(b, &a);
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_brif(
    c: *mut Cl,
    cond: Val,
    t: BlockId,
    targs: *const Val,
    tn: u32,
    e: BlockId,
    eargs: *const Val,
    en: u32,
) {
    let c = cl(c);
    let (ta, ea) = (block_args(c, targs, tn), block_args(c, eargs, en));
    let cond = val(c, cond);
    let (t, e) = (c.blocks[t.0 as usize], c.blocks[e.0 as usize]);
    bld(c).ins().brif(cond, t, &ta, e, &ea);
}

/// A multi-way branch on integer `v`: `keys[i]` goes to `targets[i]`, anything else to `default`. Targets take no
/// block arguments (jump through a trampoline block that passes them).
#[no_mangle]
pub unsafe extern "C" fn ms_cl_switch(c: *mut Cl, v: Val, default: BlockId, keys: *const u64, targets: *const BlockId, n: u32) {
    let c = cl(c);
    let (keys, targets) = (list(keys, n), list(targets, n));
    let mut sw = Switch::new();
    for (k, t) in keys.into_iter().zip(targets) {
        sw.set_entry(k as u128, c.blocks[t.0 as usize]);
    }
    let (v, d) = (val(c, v), c.blocks[default.0 as usize]);
    sw.emit(bld(c), v, d);
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_ret(c: *mut Cl, args: *const Val, n: u32) {
    let c = cl(c);
    let vs = values(c, args, n);
    bld(c).ins().return_(&vs);
}

/// A trap with a user code in 1..=250 (the runtime maps the signal to a fault).
#[no_mangle]
pub unsafe extern "C" fn ms_cl_trap(c: *mut Cl, code: u8) {
    bld(cl(c)).ins().trap(TrapCode::unwrap_user(code));
}

/// Stores the results of `inst` through `rets`, which has room for as many values as the callee's signature returns.
unsafe fn results(c: &mut Cl, inst: ir::Inst, rets: *mut Val) {
    let rs: Vec<Value> = bld(c).inst_results(inst).to_vec();
    for (i, v) in rs.into_iter().enumerate() {
        *rets.add(i) = push(c, v);
    }
}

/// Calls function `func`; the results go to `rets`, one per value its signature returns (null when none).
#[no_mangle]
pub unsafe extern "C" fn ms_cl_call(c: *mut Cl, func: Func, args: *const Val, n: u32, rets: *mut Val) {
    let c = cl(c);
    let a = values(c, args, n);
    let r = fref(c, func);
    let inst = bld(c).ins().call(r, &a);
    results(c, inst, rets)
}

/// Calls the code address `callee` with signature `sig`.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_call_indirect(c: *mut Cl, sig: SigId, callee: Val, args: *const Val, n: u32, rets: *mut Val) {
    let c = cl(c);
    let a = values(c, args, n);
    let callee = val(c, callee);
    let sref = match c.srefs.get(&sig.0) {
        Some(s) => *s,
        None => {
            let sg = c.sigs[sig.0 as usize].clone();
            let s = bld(c).import_signature(sg);
            c.srefs.insert(sig.0, s);
            s
        }
    };
    let inst = bld(c).ins().call_indirect(sref, callee, &a);
    results(c, inst, rets)
}

/// The Cranelift IR of the function being built (or just ended), valid until the next call on `c`.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_dump(c: *mut Cl) -> *const c_char {
    let c = cl(c);
    c.dump = CString::new(c.ctx.func.display().to_string().replace('\0', " ")).unwrap();
    c.dump.as_ptr()
}

/// Finishes and compiles the function. Returns 0, or 1 with the message in `ms_cl_error`.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_func_end(c: *mut Cl) -> i32 {
    let c = cl(c);
    let m = c.module.as_mut().unwrap();
    let cfg = m.isa().frontend_config();
    c.builder.take().unwrap().finalize(cfg);
    let id = c.funcs[c.cur as usize];
    match m.define_function(id, &mut c.ctx) {
        Ok(()) => {
            let code = c.ctx.compiled_code().unwrap();
            let info = &mut c.info[c.cur as usize];
            info.size = code.code_buffer().len() as u32;
            info.srclocs = code
                .buffer
                .get_srclocs_sorted()
                .iter()
                .map(|r| ClSrcLoc { start: r.start, end: r.end, loc: r.loc.bits() })
                .collect();
            0
        }
        Err(e) => {
            let text = format!("{e:?}");
            set_err(c, text);
            1
        }
    }
}

// ---- Output ----------------------------------------------------------------------------------

/// Writes the object. The bytes stay valid until `ms_cl_free`. After this the module accepts no more input.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_finish(c: *mut Cl, len: *mut usize) -> *const u8 {
    let c = cl(c);
    c.builder = None;
    let product = c.module.take().expect("finished twice").finish();
    for (i, id) in c.funcs.iter().enumerate() {
        c.info[i].offset = product.object.symbol(product.function_symbol(*id)).value;
    }
    match product.emit() {
        Ok(bytes) => {
            c.object = bytes;
            *len = c.object.len();
            c.object.as_ptr()
        }
        Err(e) => {
            set_err(c, e);
            *len = 0;
            std::ptr::null()
        }
    }
}

/// Offset of `func` in the object's text section and its size in bytes; valid after `ms_cl_finish`.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_func_info(c: *mut Cl, func: Func, offset: *mut u64, size: *mut u32) {
    let i = &cl(c).info[func.0 as usize];
    *offset = i.offset;
    *size = i.size;
}

/// The source-location ranges of `func` (offsets from its start), sorted by start.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_func_srclocs(c: *mut Cl, func: Func, n: *mut u32) -> *const ClSrcLoc {
    let i = &cl(c).info[func.0 as usize];
    *n = i.srclocs.len() as u32;
    i.srclocs.as_ptr()
}
