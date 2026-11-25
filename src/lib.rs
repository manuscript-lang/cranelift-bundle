//! Spike: a C ABI over Cranelift. Integer-only subset: enough to lower the scalar MIR ops.
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, AbiParam, Block, InstBuilder, Signature, UserFuncName, Value};
use cranelift_codegen::isa::{self, CallConv};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::ffi::CStr;
use std::os::raw::c_char;

pub struct Cl {
    module: ObjectModule,
    ctx: Context,
    fctx: FunctionBuilderContext,
    funcs: Vec<FuncId>,
    blocks: Vec<Block>,
    values: Vec<Value>,
    builder: Option<FunctionBuilder<'static>>,
    cur: usize,
    out: Vec<u8>,
}

fn b(c: &mut Cl) -> &mut FunctionBuilder<'static> {
    c.builder.as_mut().unwrap()
}

#[no_mangle]
pub extern "C" fn ms_cl_new(speed: i32) -> *mut Cl {
    let mut flags = settings::builder();
    flags.set("opt_level", if speed != 0 { "speed" } else { "none" }).unwrap();
    flags.set("is_pic", "true").unwrap();
    flags.set("unwind_info", "false").unwrap();
    flags.set("enable_verifier", "false").unwrap();
    flags.set("preserve_frame_pointers", "true").unwrap();
    let isa = isa::lookup(target_lexicon::Triple::host()).unwrap().finish(settings::Flags::new(flags)).unwrap();
    let ob = ObjectBuilder::new(isa, "ms", cranelift_module::default_libcall_names()).unwrap();
    Box::into_raw(Box::new(Cl {
        module: ObjectModule::new(ob),
        ctx: Context::new(),
        fctx: FunctionBuilderContext::new(),
        funcs: vec![],
        blocks: vec![],
        values: vec![],
        builder: None,
        cur: 0,
        out: vec![],
    }))
}

fn sig(c: &Cl, n: u32) -> Signature {
    let mut s = Signature::new(CallConv::SystemV);
    let _ = c;
    for _ in 0..n {
        s.params.push(AbiParam::new(types::I64));
    }
    s.returns.push(AbiParam::new(types::I64));
    s
}

/// Declares a function with `n` i64 parameters and an i64 result; `import` links it from outside.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_declare(c: *mut Cl, name: *const c_char, n: u32, import: i32) -> u32 {
    let c = &mut *c;
    let s = sig(c, n);
    let name = CStr::from_ptr(name).to_str().unwrap();
    let id = c.module.declare_function(name, if import != 0 { Linkage::Import } else { Linkage::Export }, &s).unwrap();
    c.funcs.push(id);
    (c.funcs.len() - 1) as u32
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_begin(c: *mut Cl, func: u32, n: u32) {
    let c = &mut *c;
    c.module.clear_context(&mut c.ctx);
    c.ctx.func.signature = sig(c, n);
    c.ctx.func.name = UserFuncName::user(0, func);
    c.cur_set(func as usize);
    c.blocks.clear();
    c.values.clear();
    let f: &'static mut cranelift_codegen::ir::Function = std::mem::transmute(&mut c.ctx.func);
    let fc: &'static mut FunctionBuilderContext = std::mem::transmute(&mut c.fctx);
    c.builder = Some(FunctionBuilder::new(f, fc));
}

impl Cl {
    fn cur_set(&mut self, f: usize) {
        self.cur = f;
    }
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_block(c: *mut Cl) -> u32 {
    let c = &mut *c;
    let blk = b(c).create_block();
    c.blocks.push(blk);
    (c.blocks.len() - 1) as u32
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_block_param(c: *mut Cl, blk: u32) -> u32 {
    let c = &mut *c;
    let blk = c.blocks[blk as usize];
    let v = b(c).append_block_param(blk, types::I64);
    c.values.push(v);
    (c.values.len() - 1) as u32
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_enter(c: *mut Cl, blk: u32, entry: i32) {
    let c = &mut *c;
    let blk = c.blocks[blk as usize];
    if entry != 0 {
        b(c).append_block_params_for_function_params(blk);
    }
    b(c).switch_to_block(blk);
}

/// Parameter `i` of the current function after `ms_cl_enter(entry)`.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_param(c: *mut Cl, blk: u32, i: u32) -> u32 {
    let c = &mut *c;
    let blk = c.blocks[blk as usize];
    let v = b(c).block_params(blk)[i as usize];
    c.values.push(v);
    (c.values.len() - 1) as u32
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_iconst(c: *mut Cl, k: i64) -> u32 {
    let c = &mut *c;
    let v = b(c).ins().iconst(types::I64, k);
    c.values.push(v);
    (c.values.len() - 1) as u32
}

/// op: 0 add 1 sub 2 mul 3 and 4 or 5 xor 6 shl 7 sshr 8 ushr.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_bin(c: *mut Cl, op: u32, x: u32, y: u32) -> u32 {
    let c = &mut *c;
    let (x, y) = (c.values[x as usize], c.values[y as usize]);
    let ins = b(c).ins();
    let v = match op {
        0 => ins.iadd(x, y),
        1 => ins.isub(x, y),
        2 => ins.imul(x, y),
        3 => ins.band(x, y),
        4 => ins.bor(x, y),
        5 => ins.bxor(x, y),
        6 => ins.ishl(x, y),
        7 => ins.sshr(x, y),
        _ => ins.ushr(x, y),
    };
    c.values.push(v);
    (c.values.len() - 1) as u32
}

/// Checked op: op 0 add 1 sub 2 mul. Returns the result; `*flag` is the overflow value (i8).
#[no_mangle]
pub unsafe extern "C" fn ms_cl_chk(c: *mut Cl, op: u32, x: u32, y: u32, flag: *mut u32) -> u32 {
    let c = &mut *c;
    let (x, y) = (c.values[x as usize], c.values[y as usize]);
    let ins = b(c).ins();
    let (v, f) = match op {
        0 => ins.sadd_overflow(x, y),
        1 => ins.ssub_overflow(x, y),
        _ => ins.smul_overflow(x, y),
    };
    c.values.push(v);
    c.values.push(f);
    *flag = (c.values.len() - 1) as u32;
    (c.values.len() - 2) as u32
}

/// cc: 0 eq 1 ne 2 lt 3 le 4 gt 5 ge (signed). The result is widened to i64.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_icmp(c: *mut Cl, cc: u32, x: u32, y: u32) -> u32 {
    let c = &mut *c;
    let (x, y) = (c.values[x as usize], c.values[y as usize]);
    let cc = [IntCC::Equal, IntCC::NotEqual, IntCC::SignedLessThan, IntCC::SignedLessThanOrEqual,
        IntCC::SignedGreaterThan, IntCC::SignedGreaterThanOrEqual][cc as usize];
    let r = b(c).ins().icmp(cc, x, y);
    let v = b(c).ins().uextend(types::I64, r);
    c.values.push(v);
    (c.values.len() - 1) as u32
}

unsafe fn args(c: &Cl, a: *const u32, n: u32) -> Vec<cranelift_codegen::ir::BlockArg> {
    (0..n).map(|i| cranelift_codegen::ir::BlockArg::Value(c.values[*a.add(i as usize) as usize])).collect()
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_jump(c: *mut Cl, blk: u32, a: *const u32, n: u32) {
    let c = &mut *c;
    let args = args(c, a, n);
    let blk = c.blocks[blk as usize];
    b(c).ins().jump(blk, &args);
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_brif(c: *mut Cl, cond: u32, t: u32, ta: *const u32, tn: u32, e: u32, ea: *const u32, en: u32) {
    let c = &mut *c;
    let (targs, eargs) = (args(c, ta, tn), args(c, ea, en));
    let cond = c.values[cond as usize];
    let (t, e) = (c.blocks[t as usize], c.blocks[e as usize]);
    b(c).ins().brif(cond, t, &targs, e, &eargs);
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_call(c: *mut Cl, func: u32, a: *const u32, n: u32) -> u32 {
    let c = &mut *c;
    let id = c.funcs[func as usize];
    let fr = {
        let bld = c.builder.as_mut().unwrap();
        c.module.declare_func_in_func(id, bld.func)
    };
    let vals: Vec<Value> = (0..n).map(|i| c.values[*a.add(i as usize) as usize]).collect();
    let inst = b(c).ins().call(fr, &vals);
    let v = b(c).inst_results(inst)[0];
    c.values.push(v);
    (c.values.len() - 1) as u32
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_ret(c: *mut Cl, v: u32) {
    let c = &mut *c;
    let v = c.values[v as usize];
    b(c).ins().return_(&[v]);
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_seal_all(c: *mut Cl) {
    b(&mut *c).seal_all_blocks();
}

/// Finishes the function: `cold` marks blocks laid out of line.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_cold(c: *mut Cl, blk: u32) {
    let c = &mut *c;
    let blk = c.blocks[blk as usize];
    b(c).set_cold_block(blk);
}

#[no_mangle]
pub unsafe extern "C" fn ms_cl_end(c: *mut Cl) {
    let c = &mut *c;
    let cfg = c.module.isa().frontend_config();
    c.builder.take().unwrap().finalize(cfg);
    let id = c.funcs[c.cur];
    c.module.define_function(id, &mut c.ctx).unwrap();
}

/// Writes the object; returns its bytes (valid until the next call) and length.
#[no_mangle]
pub unsafe extern "C" fn ms_cl_finish(c: *mut Cl, len: *mut usize) -> *const u8 {
    let c = Box::from_raw(c);
    let c = *c;
    c.module.finish().emit().map(|v| {
        let leaked = Box::leak(v.into_boxed_slice());
        *len = leaked.len();
        leaked.as_ptr()
    }).unwrap()
}
