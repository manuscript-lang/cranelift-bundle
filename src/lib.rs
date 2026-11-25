//! A C ABI over Cranelift: everything a compiler back end needs to build functions and data,
//! and write one relocatable object with line-table DWARF. See `include/cranelift.h`, which this file must match.
//!
//! Two kinds of object: a module (`Cl`: signatures, function and data declarations, data, the finished object)
//! and a function builder (`Fnb`: one function's blocks, values and compile). Builders do not touch the module while
//! they build or compile, so several threads can each own a builder, compile in parallel, and hand the results to the
//! module (`cl_define`) one at a time. Declarations must exist before a body mentions them.
//!
//! The functions never unwind across the boundary (`panic = "abort"`). A bad handle aborts the process: the caller is a
//! compiler, not an untrusted client. Failures that depend on the input (the verifier, an unknown setting) are reported
//! through `cl_error` and `cl_fn_error`.

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    self, types, AbiParam, AtomicRmwOp, Block, BlockArg, FuncRef, GlobalValue, InstBuilder, MemFlagsData, SigRef,
    Signature, SourceLoc, StackSlot, StackSlotData, StackSlotKind, TrapCode, Type, UserFuncName, Value,
};
use cranelift_codegen::isa::{self, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module, ModuleReloc};
use cranelift_object::object::write::{Relocation, SectionId, StandardSegment, SymbolId, SymbolSection};
use cranelift_object::object::{BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, SectionKind};
use cranelift_object::{ObjectBuilder, ObjectModule, ObjectProduct};
use gimli::write::{
    Address, AttributeValue, DwarfUnit, EndianVec, Expression, LineProgram, LineString, Location, LocationList, Range,
    RangeList, RelocateWriter,
    Relocation as DwarfRelocation, RelocationTarget, Sections,
};
use gimli::{Encoding, Format, LineEncoding, RunTimeEndian};
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::str::FromStr;
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};
use std::thread;

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

/// One range of a function's code and the source place of its instructions: `off..end` are offsets from the function's
/// start, the rest is what `cl_srcloc` was given (file `u32::MAX` when none was).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClCodeRange {
    pub off: u32,
    pub end: u32,
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

/// A pointer-sized relocation in a data object: at `offset` store the address of function or data `target` plus
/// `addend`. `kind` 0 is a function, 1 is data.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClReloc {
    pub offset: u32,
    pub kind: u32,
    pub target: u32,
    pub addend: i64,
}

struct FuncDecl {
    id: FuncId,
    sig: u32,
    colocated: bool,
}

struct DataDecl {
    id: DataId,
    colocated: bool,
    tls: bool,
}

/// What builders read from the module: written by the module's declare calls, read by `cl_call` and friends.
#[derive(Default)]
struct Decls {
    sigs: Vec<Signature>,
    funcs: Vec<FuncDecl>,
    datas: Vec<DataDecl>,
}

type Shared = Arc<RwLock<Decls>>;

/// A named value for the debugger, from `cl_var`: its type is a base type of `size` bytes and DWARF `encoding`.
#[derive(Clone)]
struct VarDecl {
    name: String,
    type_name: String,
    size: u32,
    encoding: u32,
    param: bool,
    label: ir::ValueLabel,
}

/// Where a named value lives: a register (DWARF number) or a frame slot (offset from the canonical frame address).
#[derive(Clone, Copy, PartialEq)]
enum VarLoc {
    Reg(u16),
    Cfa(i64),
}

/// A named value and the code ranges (offsets from the function start) where Cranelift kept it in one place.
struct VarInfo {
    decl: VarDecl,
    ranges: Vec<(u32, u32, VarLoc)>,
}

#[derive(Default)]
struct FuncInfo {
    size: u32,
    ranges: Vec<ClCodeRange>,
    vars: Vec<VarInfo>,
    /// Windows unwind data (an `.xdata` record), which the system's stack walker needs for the function.
    unwind: Option<Vec<u8>>,
    name: Option<String>,
    defined: bool,
    /// Offset in the object's text section, set by `finish`.
    offset: u64,
}

/// A function address plus `addend` in a data object, written after `finish` (Cranelift's data relocations carry no
/// addend for functions, and a table of code addresses may point into the middle of functions).
struct LateReloc {
    data: u32,
    offset: u32,
    func: u32,
    addend: i64,
}

pub struct Cl {
    module: Option<ObjectModule>,
    isa: OwnedTargetIsa,
    decls: Shared,
    run: Arc<Run>,
    late: Vec<LateReloc>,
    files: Vec<(u32, String)>,
    comp_dir: String,
    object: Vec<u8>,
    err: CString,
}

/// A function builder: owns everything one function needs, so builders on different threads do not meet.
pub struct Fnb {
    isa: OwnedTargetIsa,
    decls: Shared,
    run: Arc<Run>,
    ctx: Context,
    fctx: FunctionBuilderContext,
    builder: Option<FunctionBuilder<'static>>,
    cur: u32,
    blocks: Vec<Block>,
    vals: Vec<Value>,
    slots: Vec<StackSlot>,
    frefs: HashMap<u32, FuncRef>,
    grefs: HashMap<u32, GlobalValue>,
    srefs: HashMap<u32, SigRef>,
    /// The (file, line, column) of each `cl_srcloc` call; a `SourceLoc` is an index into it.
    locs: Vec<(u32, u32, u32)>,
    loc_ix: HashMap<(u32, u32, u32), u32>,
    /// The values `cl_var` named in the current function.
    vars: Vec<VarDecl>,
    err: CString,
    dump: CString,
}

struct Compiled {
    code: Vec<u8>,
    align: u64,
    relocs: Vec<ModuleReloc>,
}

/// One function waiting for a worker: its finished IR and what turns source places into ranges.
struct Job {
    ctx: Context,
    func: u32,
    id: FuncId,
    locs: Vec<(u32, u32, u32)>,
    vars: Vec<VarDecl>,
}

/// Compilation state shared by the module, its builders and the workers.
struct Run {
    isa: OwnedTargetIsa,
    /// Worker threads (0 or 1: compile on the calling thread).
    threads: usize,
    /// The queue and the workers, started by the first job.
    pool: Mutex<Pool>,
    info: Mutex<Vec<FuncInfo>>,
    /// Compiled functions waiting for `cl_finish`, which defines them in declaration order so the object does not
    /// depend on which worker finished first.
    done: Mutex<Vec<Option<Compiled>>>,
    errors: Mutex<Vec<(u32, String)>>,
    /// Jobs queued or running; `idle` is signalled when it reaches zero.
    pending: Mutex<usize>,
    idle: Condvar,
    /// Contexts kept for reuse: their allocations are most of the cost of starting a function.
    spare: Mutex<Vec<Context>>,
}

#[derive(Default)]
struct Pool {
    sender: Option<mpsc::SyncSender<Job>>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl Run {
    fn compile(&self, job: Job) {
        let Job { mut ctx, func, id, locs, vars } = job;
        match compile_ctx(&self.isa, &mut ctx, id, &locs, vars) {
            Ok((compiled, ranges, vars, unwind)) => {
                let mut info = self.info.lock().unwrap();
                let i = &mut info[func as usize];
                i.size = compiled.code.len() as u32;
                i.ranges = ranges;
                i.vars = vars;
                i.unwind = unwind;
                i.defined = true;
                drop(info);
                self.done.lock().unwrap()[func as usize] = Some(compiled);
            }
            Err(e) => {
                let name = self.info.lock().unwrap()[func as usize].name.clone().unwrap_or_default();
                self.errors.lock().unwrap().push((func, format!("{name}: {e}")));
            }
        }
        ctx.clear();
        let mut spare = self.spare.lock().unwrap();
        if spare.len() < 64 {
            spare.push(ctx);
        }
        drop(spare);
        let mut n = self.pending.lock().unwrap();
        *n -= 1;
        if *n == 0 {
            self.idle.notify_all();
        }
    }

    fn submit(self: &Arc<Self>, job: Job) {
        *self.pending.lock().unwrap() += 1;
        if self.threads <= 1 {
            self.compile(job);
            return;
        }
        let sender = {
            let mut pool = self.pool.lock().unwrap();
            if pool.sender.is_none() {
                // A bounded queue: a lowering that outruns the workers waits instead of piling up IR.
                let (tx, rx) = mpsc::sync_channel::<Job>(self.threads * 4);
                let rx = Arc::new(Mutex::new(rx));
                for _ in 0..self.threads {
                    let (run, rx) = (self.clone(), rx.clone());
                    let handle = thread::Builder::new().stack_size(8 << 20).spawn(move || loop {
                        let job = rx.lock().unwrap().recv();
                        match job {
                            Ok(job) => run.compile(job),
                            Err(_) => break,
                        }
                    });
                    pool.workers.push(handle.expect("worker thread"));
                }
                pool.sender = Some(tx);
            }
            pool.sender.clone().unwrap()
        };
        sender.send(job).expect("workers are running");
    }

    fn wait(&self) {
        let mut n = self.pending.lock().unwrap();
        while *n > 0 {
            n = self.idle.wait(n).unwrap();
        }
    }
}

impl Drop for Cl {
    fn drop(&mut self) {
        let mut pool = self.run.pool.lock().unwrap();
        pool.sender = None; // the workers see the queue close and end
        for w in pool.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn compile_ctx(isa: &OwnedTargetIsa, ctx: &mut Context, id: FuncId, locs: &[(u32, u32, u32)], decls: Vec<VarDecl>) -> Result<(Compiled, Vec<ClCodeRange>, Vec<VarInfo>, Option<Vec<u8>>), String> {
    // A Cranelift bug panics; report it as this function's error so the caller can fall back instead of dying.
    let compiled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.compile(&**isa, &mut ControlPlane::default()).map(|_| ()).map_err(|e| format!("{e:?}"))));
    match compiled {
        Ok(result) => result?,
        Err(payload) => {
            let text = payload.downcast_ref::<String>().cloned().or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()));
            return Err(format!("Cranelift panicked: {}", text.unwrap_or_default()));
        }
    };
    let cc = ctx.compiled_code().unwrap();
    let size = cc.code_buffer().len() as u32;
    let mut ranges = Vec::new();
    for r in cc.buffer.get_srclocs_sorted() {
        let (file, line, column) = locs.get(r.loc.bits() as usize).copied().unwrap_or((u32::MAX, 0, 0));
        ranges.push(ClCodeRange { off: r.start, end: r.end.min(size), file, line, column });
    }
    let relocs = cc.buffer.relocs().iter().map(|r| ModuleReloc::from_mach_reloc(r, &ctx.func, id)).collect();
    let vars = decls.into_iter().map(|decl| var_info(isa, cc, decl)).collect();
    let unwind = windows_unwind(isa, cc, size)?;
    Ok((Compiled { code: cc.code_buffer().to_vec(), align: cc.buffer.alignment as u64, relocs }, ranges, vars, unwind))
}

/// The `.xdata` record of a function on Windows, where the system's stack walker finds frames only through unwind data
/// (none elsewhere: the frame-pointer chain and the DWARF tables serve there).
fn windows_unwind(isa: &OwnedTargetIsa, cc: &cranelift_codegen::CompiledCode, size: u32) -> Result<Option<Vec<u8>>, String> {
    use cranelift_codegen::isa::unwind::UnwindInfo;
    if isa.triple().operating_system != target_lexicon::OperatingSystem::Windows {
        return Ok(None);
    }
    match cc.create_unwind_info(&**isa).map_err(|e| format!("unwind info: {e:?}"))? {
        Some(UnwindInfo::WindowsX64(info)) => {
            let mut bytes = vec![0u8; info.emit_size()];
            info.emit(&mut bytes);
            Ok(Some(bytes))
        }
        Some(UnwindInfo::WindowsArm64(info)) => {
            // One header word, then the codes, with no epilogue scopes: an unwinder that was told of one takes the
            // function's last instructions for it, and Cranelift puts cold blocks that call the fault helpers
            // there, so a frame's caller was lost. Cranelift does not write the end code (0xE4) the unwinder
            // stops at: a final word of it follows the codes, whose zero padding is harmless no-op allocations.
            let words = u32::from(info.code_words()) + 1;
            if words > 31 || size / 4 >= 1 << 18 {
                return Err("function too large for compact Windows unwind data".to_string());
            }
            let mut codes = vec![0u8; words as usize * 4];
            info.emit(&mut codes[..words as usize * 4 - 4]);
            codes[words as usize * 4 - 4] = 0xE4;
            let header = size / 4 | words << 27;
            let mut bytes = header.to_le_bytes().to_vec();
            bytes.extend(codes);
            Ok(Some(bytes))
        }
        _ => Ok(None),
    }
}

/// The places Cranelift tracked for a variable's label, in code order, with neighbours that stay put joined (it splits
/// a range at every block).
fn var_info(isa: &OwnedTargetIsa, cc: &cranelift_codegen::CompiledCode, decl: VarDecl) -> VarInfo {
    let mut found: Vec<(u32, u32, VarLoc)> = cc
        .value_labels_ranges
        .get(&decl.label)
        .map(|rs| {
            rs.iter()
                .filter_map(|r| {
                    let loc = match r.loc {
                        cranelift_codegen::LabelValueLoc::Reg(reg) => VarLoc::Reg(isa.map_regalloc_reg_to_dwarf(reg).ok()?),
                        cranelift_codegen::LabelValueLoc::CFAOffset(o) => VarLoc::Cfa(o),
                    };
                    Some((r.start, r.end, loc))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort_by_key(|r| r.0);
    let mut ranges: Vec<(u32, u32, VarLoc)> = Vec::new();
    for r in found {
        match ranges.last_mut() {
            Some(last) if last.1 == r.0 && last.2 == r.2 => last.1 = r.1,
            _ => ranges.push(r),
        }
    }
    // A parameter is where the calling convention put it from the first instruction: Cranelift's range starts after the
    // frame is set up, so a debugger stopped at the function's address would find it nowhere.
    if decl.param {
        if let Some(first) = ranges.first_mut() {
            first.0 = 0;
        }
    }
    VarInfo { decl, ranges }
}

fn cl<'a>(c: *mut Cl) -> &'a mut Cl {
    unsafe { &mut *c }
}

fn fb<'a>(f: *mut Fnb) -> &'a mut Fnb {
    unsafe { &mut *f }
}

fn bld(c: &mut Fnb) -> &mut FunctionBuilder<'static> {
    c.builder.as_mut().expect("cl_fn_begin was not called")
}

fn module(c: &mut Cl) -> &mut ObjectModule {
    c.module.as_mut().expect("the module was finished")
}

fn cstr<'a>(p: *const c_char) -> &'a str {
    unsafe { CStr::from_ptr(p) }.to_str().expect("a name is UTF-8")
}

fn cerr(msg: impl std::fmt::Display) -> CString {
    CString::new(msg.to_string().replace('\0', " ")).unwrap()
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

fn push(c: &mut Fnb, v: Value) -> Val {
    c.vals.push(v);
    Val((c.vals.len() - 1) as u32)
}

fn val(c: &Fnb, v: Val) -> Value {
    c.vals[v.0 as usize]
}

unsafe fn list<T: Copy>(p: *const T, n: u32) -> Vec<T> {
    if n == 0 { Vec::new() } else { std::slice::from_raw_parts(p, n as usize).to_vec() }
}

fn block_args(c: &Fnb, a: *const Val, n: u32) -> Vec<BlockArg> {
    unsafe { list(a, n) }.into_iter().map(|v| BlockArg::Value(val(c, v))).collect()
}

fn values(c: &Fnb, a: *const Val, n: u32) -> Vec<Value> {
    unsafe { list(a, n) }.into_iter().map(|v| val(c, v)).collect()
}

fn linkage(l: u32) -> Linkage {
    match l {
        0 => Linkage::Import,
        1 => Linkage::Export,
        2 => Linkage::Local,
        _ => Linkage::Preemptible,
    }
}

// ---- Module ----------------------------------------------------------------------------------

thread_local! {
    static LAST_ERROR: std::cell::RefCell<CString> = std::cell::RefCell::new(CString::default());
}

/// Creates a module for `triple` (null: the host). `keys[i]`/`vals[i]` are Cranelift settings that override the
/// defaults (for example `opt_level` = `speed`, `regalloc_algorithm` = `single_pass`). Returns null and sets the
/// error (read with `cl_error(null)`) when the target or a setting is unknown.
#[no_mangle]
pub unsafe extern "C" fn cl_new(triple: *const c_char, n: u32, keys: *const *const c_char, vals: *const *const c_char) -> *mut Cl {
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
        set("unwind_info", if triple.operating_system == target_lexicon::OperatingSystem::Windows { "true" } else { "false" })?;
        set("enable_verifier", "false")?;
        set("preserve_frame_pointers", "true")?;
        set("enable_multi_ret_implicit_sret", "true")?;
        set("enable_probestack", "true")?;
        set("probestack_strategy", "inline")?;
        // `threads` is ours, not Cranelift's: how many workers compile functions (default: one per core).
        let mut threads = thread::available_parallelism().map_or(1, |n| n.get());
        for i in 0..n as usize {
            let (k, v) = (cstr(*keys.add(i)), cstr(*vals.add(i)));
            if k == "threads" {
                threads = v.parse().map_err(|_| format!("threads={v}: not a number"))?;
            } else {
                set(k, v)?;
            }
        }
        let isa: OwnedTargetIsa = isa::lookup(triple)
            .map_err(|e| e.to_string())?
            .finish(settings::Flags::new(flags))
            .map_err(|e| e.to_string())?;
        let ob = ObjectBuilder::new(isa.clone(), "ms", cranelift_module::default_libcall_names()).map_err(|e| e.to_string())?;
        Ok(Cl {
            module: Some(ObjectModule::new(ob)),
            run: Arc::new(Run {
                isa: isa.clone(),
                threads,
                pool: Mutex::default(),
                info: Mutex::default(),
                done: Mutex::default(),
                errors: Mutex::default(),
                pending: Mutex::new(0),
                idle: Condvar::new(),
                spare: Mutex::default(),
            }),
            isa,
            decls: Shared::default(),
            late: vec![],
            files: vec![],
            comp_dir: String::new(),
            object: vec![],
            err: CString::default(),
        })
    })();
    match r {
        Ok(c) => Box::into_raw(Box::new(c)),
        Err(e) => {
            LAST_ERROR.with(|l| *l.borrow_mut() = cerr(e));
            std::ptr::null_mut()
        }
    }
}

/// The last error of `c`, or of `cl_new` when `c` is null. Valid until the next call on `c`.
#[no_mangle]
pub unsafe extern "C" fn cl_error(c: *mut Cl) -> *const c_char {
    if c.is_null() {
        LAST_ERROR.with(|l| l.borrow().as_ptr())
    } else {
        cl(c).err.as_ptr()
    }
}

#[no_mangle]
pub unsafe extern "C" fn cl_free(c: *mut Cl) {
    if !c.is_null() {
        drop(Box::from_raw(c));
    }
}

/// Starts a signature. The calling convention is the target's C one.
#[no_mangle]
pub unsafe extern "C" fn cl_sig_new(c: *mut Cl) -> SigId {
    let c = cl(c);
    let cc = c.isa.default_call_conv();
    let mut d = c.decls.write().unwrap();
    d.sigs.push(Signature::new(cc));
    SigId((d.sigs.len() - 1) as u32)
}

/// Adds a parameter (`ext` 0 none, 1 zero-extend, 2 sign-extend to a full register).
#[no_mangle]
pub unsafe extern "C" fn cl_sig_param(c: *mut Cl, sig: SigId, t: u32, ext: u32) {
    cl(c).decls.write().unwrap().sigs[sig.0 as usize].params.push(abi(t, ext));
}

#[no_mangle]
pub unsafe extern "C" fn cl_sig_ret(c: *mut Cl, sig: SigId, t: u32, ext: u32) {
    cl(c).decls.write().unwrap().sigs[sig.0 as usize].returns.push(abi(t, ext));
}

/// Declares a function; `link` 0 import, 1 export, 2 local (hidden), 3 preemptible.
#[no_mangle]
pub unsafe extern "C" fn cl_declare_func(c: *mut Cl, name: *const c_char, sig: SigId, link: u32) -> Func {
    let c = cl(c);
    let s = c.decls.read().unwrap().sigs[sig.0 as usize].clone();
    let l = linkage(link);
    let id = module(c).declare_function(cstr(name), l, &s).expect("declare_function");
    let mut d = c.decls.write().unwrap();
    d.funcs.push(FuncDecl { id, sig: sig.0, colocated: l.is_final() });
    c.run.info.lock().unwrap().push(FuncInfo { name: Some(cstr(name).to_string()), ..FuncInfo::default() });
    c.run.done.lock().unwrap().push(None);
    Func((d.funcs.len() - 1) as u32)
}

/// Declares a data object (`tls` makes it thread-local).
#[no_mangle]
pub unsafe extern "C" fn cl_declare_data(c: *mut Cl, name: *const c_char, link: u32, writable: i32, tls: i32) -> Data {
    let c = cl(c);
    let l = linkage(link);
    let id = module(c).declare_data(cstr(name), l, writable != 0, tls != 0).expect("declare_data");
    let mut d = c.decls.write().unwrap();
    d.datas.push(DataDecl { id, colocated: l.is_final(), tls: tls != 0 });
    Data((d.datas.len() - 1) as u32)
}

/// Defines a data object of `len` bytes: `bytes` (null for zeros), alignment `align`, and `n` relocations.
#[no_mangle]
pub unsafe extern "C" fn cl_define_data(c: *mut Cl, data: Data, bytes: *const u8, len: u32, align: u32, relocs: *const ClReloc, n: u32) {
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
            if r.addend != 0 {
                c.late.push(LateReloc { data: data.0, offset: r.offset, func: r.target, addend: r.addend });
            } else {
                let id = c.decls.read().unwrap().funcs[r.target as usize].id;
                let f = module(c).declare_func_in_data(id, &mut d);
                d.write_function_addr(r.offset, f);
            }
        } else {
            let id = c.decls.read().unwrap().datas[r.target as usize].id;
            let g = module(c).declare_data_in_data(id, &mut d);
            d.write_data_addr(r.offset, g, r.addend);
        }
    }
    let id = c.decls.read().unwrap().datas[data.0 as usize].id;
    module(c).define_data(id, &d).expect("define_data");
}

/// Names the source file `index` for the line table; `path` is as the debugger should show it. The line table is
/// written at `cl_finish` when at least one file is named.
#[no_mangle]
pub unsafe extern "C" fn cl_dwarf_file(c: *mut Cl, index: u32, path: *const c_char) {
    cl(c).files.push((index, cstr(path).to_string()));
}

/// The compilation directory recorded in the line table.
#[no_mangle]
pub unsafe extern "C" fn cl_dwarf_dir(c: *mut Cl, dir: *const c_char) {
    cl(c).comp_dir = cstr(dir).to_string();
}

/// The name a debugger shows for `func` (default: its symbol name).
#[no_mangle]
pub unsafe extern "C" fn cl_func_name(c: *mut Cl, func: Func, name: *const c_char) {
    cl(c).run.info.lock().unwrap()[func.0 as usize].name = Some(cstr(name).to_string());
}

// ---- Function builders -----------------------------------------------------------------------

/// A builder for the functions of module `c`. One per thread that builds; reuse it function after function.
#[no_mangle]
pub unsafe extern "C" fn cl_fn_new(c: *mut Cl) -> *mut Fnb {
    let c = &*c; // shared: builders are made on the threads that build
    Box::into_raw(Box::new(Fnb {
        isa: c.isa.clone(),
        decls: c.decls.clone(),
        run: c.run.clone(),
        ctx: Context::new(),
        fctx: FunctionBuilderContext::new(),
        builder: None,
        cur: 0,
        blocks: vec![],
        vals: vec![],
        slots: vec![],
        frefs: HashMap::new(),
        grefs: HashMap::new(),
        srefs: HashMap::new(),
        locs: vec![],
        vars: vec![],
        loc_ix: HashMap::new(),
        err: CString::default(),
        dump: CString::default(),
    }))
}

#[no_mangle]
pub unsafe extern "C" fn cl_fn_free(f: *mut Fnb) {
    if !f.is_null() {
        let mut f = Box::from_raw(f);
        f.builder = None;
    }
}

#[no_mangle]
pub unsafe extern "C" fn cl_fn_error(f: *mut Fnb) -> *const c_char {
    fb(f).err.as_ptr()
}

/// Starts the body of a declared function. Create its blocks next; `cl_entry` makes the entry block.
#[no_mangle]
pub unsafe extern "C" fn cl_fn_begin(f: *mut Fnb, func: Func) {
    let c = fb(f);
    if c.builder.take().is_some() {
        // A function abandoned half built leaves its blocks in the SSA state.
        c.fctx = FunctionBuilderContext::new();
    }
    c.ctx.clear();
    {
        let d = c.decls.read().unwrap();
        c.ctx.func.signature = d.sigs[d.funcs[func.0 as usize].sig as usize].clone();
    }
    c.ctx.func.name = UserFuncName::user(0, func.0);
    c.cur = func.0;
    c.blocks.clear();
    c.vals.clear();
    c.slots.clear();
    c.frefs.clear();
    c.grefs.clear();
    c.srefs.clear();
    c.locs.clear();
    c.vars.clear();
    c.loc_ix.clear();
    // The builder borrows the context and the function for as long as the body is built.
    let func_ref: &'static mut ir::Function = std::mem::transmute(&mut c.ctx.func);
    let fc: &'static mut FunctionBuilderContext = std::mem::transmute(&mut c.fctx);
    c.builder = Some(FunctionBuilder::new(func_ref, fc));
}

/// The source place of the instructions that follow: module file index, line and column of a DWARF row.
#[no_mangle]
pub unsafe extern "C" fn cl_srcloc(f: *mut Fnb, file: u32, line: u32, column: u32) {
    let c = fb(f);
    let key = (file, line, column);
    let ix = match c.loc_ix.get(&key) {
        Some(i) => *i,
        None => {
            c.locs.push(key);
            let i = (c.locs.len() - 1) as u32;
            c.loc_ix.insert(key, i);
            i
        }
    };
    bld(c).set_srcloc(SourceLoc::new(ix));
}

/// Names `value` for the debugger: it is shown as `name`, of a base type called `type_name` of `size` bytes and DWARF
/// base-type `encoding` (for example `DW_ATE_signed`), a parameter when `param` is not 0. The value is described where
/// the code keeps it, so it must stay live in the code that follows. At most one name per value.
#[no_mangle]
pub unsafe extern "C" fn cl_var(f: *mut Fnb, value: Val, name: *const c_char, type_name: *const c_char, size: u32, encoding: u32, param: i32) {
    let c = fb(f);
    let label = ir::ValueLabel::from_u32(c.vars.len() as u32);
    let v = val(c, value);
    bld(c).func.stencil.dfg.collect_debug_info();
    bld(c).set_val_label(v, label);
    let text = |p: *const c_char| CStr::from_ptr(p).to_string_lossy().into_owned();
    c.vars.push(VarDecl { name: text(name), type_name: text(type_name), size, encoding, param: param != 0, label });
}

fn fref(c: &mut Fnb, func: Func) -> FuncRef {
    if let Some(r) = c.frefs.get(&func.0) {
        return *r;
    }
    let (id, sig, colocated) = {
        let d = c.decls.read().unwrap();
        let fd = &d.funcs[func.0 as usize];
        (fd.id, d.sigs[fd.sig as usize].clone(), fd.colocated)
    };
    let func_ir = &mut c.builder.as_mut().unwrap().func;
    let signature = func_ir.import_signature(sig);
    let name = func_ir.declare_imported_user_function(ir::UserExternalName { namespace: 0, index: id.as_u32() });
    let r = func_ir.import_function(ir::ExtFuncData { name: ir::ExternalName::user(name), signature, colocated, patchable: false });
    c.frefs.insert(func.0, r);
    r
}

fn gref(c: &mut Fnb, data: Data) -> GlobalValue {
    if let Some(g) = c.grefs.get(&data.0) {
        return *g;
    }
    let (id, colocated, tls) = {
        let d = c.decls.read().unwrap();
        let dd = &d.datas[data.0 as usize];
        (dd.id, dd.colocated, dd.tls)
    };
    let func_ir = &mut c.builder.as_mut().unwrap().func;
    let name = func_ir.declare_imported_user_function(ir::UserExternalName { namespace: 1, index: id.as_u32() });
    let g = func_ir.create_global_value(ir::GlobalValueData::Symbol {
        name: ir::ExternalName::user(name),
        offset: ir::immediates::Imm64::new(0),
        colocated,
        tls,
    });
    c.grefs.insert(data.0, g);
    g
}

#[no_mangle]
pub unsafe extern "C" fn cl_func_addr(f: *mut Fnb, func: Func) -> Val {
    let c = fb(f);
    let r = fref(c, func);
    let v = bld(c).ins().func_addr(types::I64, r);
    push(c, v)
}

/// The address of a data object (a symbol value).
#[no_mangle]
pub unsafe extern "C" fn cl_data_addr(f: *mut Fnb, data: Data) -> Val {
    let c = fb(f);
    let g = gref(c, data);
    let v = bld(c).ins().symbol_value(types::I64, g);
    push(c, v)
}

/// Stores the results of `inst` through `rets`, which has room for as many values as the callee's signature returns.
unsafe fn results(c: &mut Fnb, inst: ir::Inst, rets: *mut Val) {
    let rs: Vec<Value> = bld(c).inst_results(inst).to_vec();
    for (i, v) in rs.into_iter().enumerate() {
        *rets.add(i) = push(c, v);
    }
}

/// Calls function `func`; the results go to `rets`, one per value its signature returns (null when none).
#[no_mangle]
pub unsafe extern "C" fn cl_call(f: *mut Fnb, func: Func, args: *const Val, n: u32, rets: *mut Val) {
    let c = fb(f);
    let a = values(c, args, n);
    let r = fref(c, func);
    let inst = bld(c).ins().call(r, &a);
    results(c, inst, rets)
}

/// Calls the code address `callee` with signature `sig`.
#[no_mangle]
pub unsafe extern "C" fn cl_call_indirect(f: *mut Fnb, sig: SigId, callee: Val, args: *const Val, n: u32, rets: *mut Val) {
    let c = fb(f);
    let a = values(c, args, n);
    let callee = val(c, callee);
    let sref = match c.srefs.get(&sig.0) {
        Some(s) => *s,
        None => {
            let sg = c.decls.read().unwrap().sigs[sig.0 as usize].clone();
            let s = bld(c).import_signature(sg);
            c.srefs.insert(sig.0, s);
            s
        }
    };
    let inst = bld(c).ins().call_indirect(sref, callee, &a);
    results(c, inst, rets)
}

/// The Cranelift IR of the function being built (or just compiled), valid until the next call on `f`.
#[no_mangle]
pub unsafe extern "C" fn cl_dump(f: *mut Fnb) -> *const c_char {
    let c = fb(f);
    c.dump = cerr(c.ctx.func.display());
    c.dump.as_ptr()
}

/// Finishes the function and queues it for the module's workers (compiled on this thread when `threads` is 1). The
/// builder may begin its next function at once. Compile errors surface from `cl_wait` or `cl_finish`, named by
/// function. Returns 0.
#[no_mangle]
pub unsafe extern "C" fn cl_fn_end(f: *mut Fnb) -> i32 {
    let c = fb(f);
    let cfg = c.isa.frontend_config();
    c.builder.take().expect("cl_fn_begin was not called").finalize(cfg);
    let func = c.cur;
    let id = c.decls.read().unwrap().funcs[func as usize].id;
    let fresh = c.run.spare.lock().unwrap().pop().unwrap_or_else(Context::new);
    let ctx = std::mem::replace(&mut c.ctx, fresh);
    let locs = std::mem::take(&mut c.locs);
    let vars = std::mem::take(&mut c.vars);
    c.run.submit(Job { ctx, func, id, locs, vars });
    0
}

/// Waits for every queued function. Returns 0, or 1 with the first error (in declaration order) in `cl_error`.
#[no_mangle]
pub unsafe extern "C" fn cl_wait(c: *mut Cl) -> i32 {
    let c = cl(c);
    c.run.wait();
    let mut errors = c.run.errors.lock().unwrap();
    errors.sort();
    match errors.first() {
        Some((_, e)) => {
            c.err = cerr(e);
            1
        }
        None => 0,
    }
}

// ---- Blocks, values and instructions -----------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn cl_block_new(f: *mut Fnb) -> BlockId {
    let c = fb(f);
    let b = bld(c).create_block();
    c.blocks.push(b);
    BlockId((c.blocks.len() - 1) as u32)
}

#[no_mangle]
pub unsafe extern "C" fn cl_block_param_add(f: *mut Fnb, blk: BlockId, t: u32) -> Val {
    let c = fb(f);
    let b = c.blocks[blk.0 as usize];
    let v = bld(c).append_block_param(b, ty(t));
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_block_param(f: *mut Fnb, blk: BlockId, i: u32) -> Val {
    let c = fb(f);
    let b = c.blocks[blk.0 as usize];
    let v = bld(c).block_params(b)[i as usize];
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_entry(f: *mut Fnb) -> BlockId {
    let c = fb(f);
    let b = bld(c).create_block();
    bld(c).append_block_params_for_function_params(b);
    bld(c).switch_to_block(b);
    c.blocks.push(b);
    BlockId((c.blocks.len() - 1) as u32)
}

#[no_mangle]
pub unsafe extern "C" fn cl_enter(f: *mut Fnb, blk: BlockId) {
    let c = fb(f);
    let b = c.blocks[blk.0 as usize];
    bld(c).switch_to_block(b);
}

#[no_mangle]
pub unsafe extern "C" fn cl_seal(f: *mut Fnb, blk: BlockId) {
    let c = fb(f);
    let b = c.blocks[blk.0 as usize];
    bld(c).seal_block(b);
}

#[no_mangle]
pub unsafe extern "C" fn cl_seal_all(f: *mut Fnb) {
    bld(fb(f)).seal_all_blocks();
}

#[no_mangle]
pub unsafe extern "C" fn cl_cold(f: *mut Fnb, blk: BlockId) {
    let c = fb(f);
    let b = c.blocks[blk.0 as usize];
    bld(c).set_cold_block(b);
}

#[no_mangle]
pub unsafe extern "C" fn cl_stack_slot(f: *mut Fnb, size: u32, align_shift: u8) -> SlotId {
    let c = fb(f);
    let s = bld(c).create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, align_shift));
    c.slots.push(s);
    SlotId((c.slots.len() - 1) as u32)
}

#[no_mangle]
pub unsafe extern "C" fn cl_stack_addr(f: *mut Fnb, slot: SlotId, off: i32) -> Val {
    let c = fb(f);
    let s = c.slots[slot.0 as usize];
    let v = bld(c).ins().stack_addr(types::I64, s, off);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_sp(f: *mut Fnb) -> Val {
    let c = fb(f);
    let v = bld(c).ins().get_stack_pointer(types::I64);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_iconst(f: *mut Fnb, t: u32, k: i64) -> Val {
    let c = fb(f);
    let v = bld(c).ins().iconst(ty(t), k);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_f32const(f: *mut Fnb, k: f32) -> Val {
    let c = fb(f);
    let v = bld(c).ins().f32const(k);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_f64const(f: *mut Fnb, k: f64) -> Val {
    let c = fb(f);
    let v = bld(c).ins().f64const(k);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_unary(f: *mut Fnb, op: u32, x: Val) -> Val {
    let c = fb(f);
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

#[no_mangle]
pub unsafe extern "C" fn cl_binary(f: *mut Fnb, op: u32, x: Val, y: Val) -> Val {
    let c = fb(f);
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

#[no_mangle]
pub unsafe extern "C" fn cl_ovf(f: *mut Fnb, op: u32, x: Val, y: Val) -> Ovf {
    let c = fb(f);
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

#[no_mangle]
pub unsafe extern "C" fn cl_cast(f: *mut Fnb, op: u32, t: u32, x: Val) -> Val {
    let c = fb(f);
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

#[no_mangle]
pub unsafe extern "C" fn cl_icmp(f: *mut Fnb, cc: u32, x: Val, y: Val) -> Val {
    let c = fb(f);
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

#[no_mangle]
pub unsafe extern "C" fn cl_fcmp(f: *mut Fnb, cc: u32, x: Val, y: Val) -> Val {
    let c = fb(f);
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
pub unsafe extern "C" fn cl_select(f: *mut Fnb, cond: Val, a: Val, b: Val) -> Val {
    let c = fb(f);
    let (cond, a, b) = (val(c, cond), val(c, a), val(c, b));
    let v = bld(c).ins().select(cond, a, b);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_load(f: *mut Fnb, t: u32, flags: u32, base: Val, off: i32) -> Val {
    let c = fb(f);
    let base = val(c, base);
    let v = bld(c).ins().load(ty(t), mflags(flags), base, off);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_load_ext(f: *mut Fnb, bits: u32, signed: i32, t: u32, flags: u32, base: Val, off: i32) -> Val {
    let c = fb(f);
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
pub unsafe extern "C" fn cl_store(f: *mut Fnb, flags: u32, v: Val, base: Val, off: i32) {
    let c = fb(f);
    let (v, base) = (val(c, v), val(c, base));
    bld(c).ins().store(mflags(flags), v, base, off);
}

/// Copies `bytes` bytes (a constant) from `src` to `dst`; the ranges must not overlap. Small sizes become loads and
/// stores, larger ones a call of the C library's `memcpy`.
#[no_mangle]
pub unsafe extern "C" fn cl_copy(f: *mut Fnb, dst: Val, src: Val, bytes: u32) {
    let c = fb(f);
    let (dst, src) = (val(c, dst), val(c, src));
    let cfg = c.isa.frontend_config();
    let b = bld(c);
    if bytes <= 128 {
        b.emit_small_memory_copy(cfg, dst, src, bytes as u64, 1, 1, true, mflags(3));
    } else {
        let n = b.ins().iconst(ir::types::I64, bytes as i64);
        b.call_memcpy(cfg, dst, src, n);
    }
}

/// Copies `n` bytes (a value) from `src` to `dst`: the C library's `memmove`, so the ranges may overlap.
#[no_mangle]
pub unsafe extern "C" fn cl_copy_dyn(f: *mut Fnb, dst: Val, src: Val, n: Val) {
    let c = fb(f);
    let (dst, src, n) = (val(c, dst), val(c, src), val(c, n));
    let cfg = c.isa.frontend_config();
    bld(c).call_memmove(cfg, dst, src, n);
}

/// Sets `bytes` bytes (a constant) at `dst` to `byte`.
#[no_mangle]
pub unsafe extern "C" fn cl_fill(f: *mut Fnb, dst: Val, byte: u8, bytes: u32) {
    let c = fb(f);
    let dst = val(c, dst);
    let cfg = c.isa.frontend_config();
    let b = bld(c);
    if bytes <= 128 {
        b.emit_small_memset(cfg, dst, byte, bytes as u64, 1, mflags(3));
    } else {
        let ch = b.ins().iconst(ir::types::I8, byte as i64);
        let n = b.ins().iconst(ir::types::I64, bytes as i64);
        b.call_memset(cfg, dst, ch, n);
    }
}

#[no_mangle]
pub unsafe extern "C" fn cl_store_trunc(f: *mut Fnb, bits: u32, flags: u32, v: Val, base: Val, off: i32) {
    let c = fb(f);
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
pub unsafe extern "C" fn cl_atomic_load(f: *mut Fnb, t: u32, flags: u32, addr: Val) -> Val {
    let c = fb(f);
    let addr = val(c, addr);
    let v = bld(c).ins().atomic_load(ty(t), mflags(flags), addr);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_atomic_store(f: *mut Fnb, flags: u32, v: Val, addr: Val) {
    let c = fb(f);
    let (v, addr) = (val(c, v), val(c, addr));
    bld(c).ins().atomic_store(mflags(flags), v, addr);
}

#[no_mangle]
pub unsafe extern "C" fn cl_atomic_rmw(f: *mut Fnb, op: u32, t: u32, flags: u32, addr: Val, x: Val) -> Val {
    let c = fb(f);
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

#[no_mangle]
pub unsafe extern "C" fn cl_atomic_cas(f: *mut Fnb, flags: u32, addr: Val, expected: Val, new: Val) -> Val {
    let c = fb(f);
    let (addr, e, n) = (val(c, addr), val(c, expected), val(c, new));
    let v = bld(c).ins().atomic_cas(mflags(flags), addr, e, n);
    push(c, v)
}

#[no_mangle]
pub unsafe extern "C" fn cl_fence(f: *mut Fnb) {
    bld(fb(f)).ins().fence();
}

#[no_mangle]
pub unsafe extern "C" fn cl_jump(f: *mut Fnb, blk: BlockId, args: *const Val, n: u32) {
    let c = fb(f);
    let a = block_args(c, args, n);
    let b = c.blocks[blk.0 as usize];
    bld(c).ins().jump(b, &a);
}

#[no_mangle]
pub unsafe extern "C" fn cl_brif(
    f: *mut Fnb,
    cond: Val,
    t: BlockId,
    targs: *const Val,
    tn: u32,
    e: BlockId,
    eargs: *const Val,
    en: u32,
) {
    let c = fb(f);
    let (ta, ea) = (block_args(c, targs, tn), block_args(c, eargs, en));
    let cond = val(c, cond);
    let (t, e) = (c.blocks[t.0 as usize], c.blocks[e.0 as usize]);
    bld(c).ins().brif(cond, t, &ta, e, &ea);
}

#[no_mangle]
pub unsafe extern "C" fn cl_switch(f: *mut Fnb, v: Val, default: BlockId, keys: *const u64, targets: *const BlockId, n: u32) {
    let c = fb(f);
    let (keys, targets) = (list(keys, n), list(targets, n));
    let mut sw = Switch::new();
    for (k, t) in keys.into_iter().zip(targets) {
        sw.set_entry(k as u128, c.blocks[t.0 as usize]);
    }
    let (v, d) = (val(c, v), c.blocks[default.0 as usize]);
    sw.emit(bld(c), v, d);
}

#[no_mangle]
pub unsafe extern "C" fn cl_ret(f: *mut Fnb, args: *const Val, n: u32) {
    let c = fb(f);
    let vs = values(c, args, n);
    bld(c).ins().return_(&vs);
}

#[no_mangle]
pub unsafe extern "C" fn cl_trap(f: *mut Fnb, code: u8) {
    bld(fb(f)).ins().trap(TrapCode::unwrap_user(code));
}


// ---- Output ----------------------------------------------------------------------------------

#[derive(Clone)]
struct Section {
    bytes: EndianVec<RunTimeEndian>,
    relocations: Vec<DwarfRelocation>,
}

impl RelocateWriter for Section {
    type Writer = EndianVec<RunTimeEndian>;
    fn writer(&self) -> &Self::Writer {
        &self.bytes
    }
    fn writer_mut(&mut self) -> &mut Self::Writer {
        &mut self.bytes
    }
    fn relocate(&mut self, r: DwarfRelocation) {
        self.relocations.push(r);
    }
}

/// The address each function's section will have in a Mach-O object. The object lays its sections out in order
/// from zero and the debug sections added after these do not move them, so a trial write tells.
fn section_addresses(product: &ObjectProduct, symbols: &[SymbolId]) -> Result<HashMap<SectionId, u64>, String> {
    use cranelift_object::object::{Object, ObjectSection};
    let bytes = product.object.write().map_err(|e| format!("object: {e}"))?;
    let file = cranelift_object::object::File::parse(&*bytes).map_err(|e| format!("object: {e}"))?;
    let mut found = HashMap::new();
    for &symbol in symbols {
        let Some(id) = product.object.symbol(symbol).section.id() else { continue };
        let Some(name) = product.object.section(id).name() else { continue };
        if let Some(section) = file.sections().find(|s| s.name().ok() == Some(name)) {
            found.insert(id, section.address());
        }
    }
    Ok(found)
}

/// Writes the line table (DWARF 4, a compile unit with a subprogram per function) into the object, with relocations
/// against the function symbols. A row's column is the value given to `cl_srcloc`.
fn emit_dwarf(c: &Cl, product: &mut ObjectProduct, ids: &[FuncId]) -> Result<(), String> {
    if c.files.is_empty() {
        return Ok(());
    }
    let encoding = Encoding { format: Format::Dwarf32, version: 4, address_size: 8 };
    let first = c.files[0].1.clone();
    let mut dwarf = DwarfUnit::new(encoding);
    let mut lines = LineProgram::new(
        encoding,
        LineEncoding::default(),
        LineString::String(c.comp_dir.clone().into_bytes()),
        None,
        LineString::String(first.clone().into_bytes()),
        None,
    );
    let dir = lines.default_directory();
    let mut files: HashMap<u32, gimli::write::FileId> = HashMap::new();
    for (ix, path) in &c.files {
        files.insert(*ix, lines.add_file(LineString::String(path.clone().into_bytes()), dir, None));
    }
    let root = dwarf.unit.root();
    // Range lists hold absolute addresses, so the base is zero; dsymutil wants the explicit original base.
    dwarf.unit.get_mut(root).set(gimli::DW_AT_low_pc, AttributeValue::Address(Address::Constant(0)));
    dwarf.unit.get_mut(root).set(gimli::DW_AT_name, AttributeValue::String(first.into_bytes()));
    dwarf.unit.get_mut(root).set(gimli::DW_AT_comp_dir, AttributeValue::String(c.comp_dir.clone().into_bytes()));
    dwarf.unit.get_mut(root).set(gimli::DW_AT_producer, AttributeValue::String(b"Cranelift".to_vec()));
    let mut symbols = Vec::new();
    let mut ranges = Vec::new();
    let mut types: HashMap<(String, u32, u32), gimli::write::UnitEntryId> = HashMap::new();
    let infos = c.run.info.lock().unwrap();
    for (i, info) in infos.iter().enumerate() {
        if !info.defined || info.ranges.is_empty() {
            continue;
        }
        let symbol = symbols.len();
        symbols.push(product.function_symbol(ids[i]));
        let address = Address::Symbol { symbol, addend: 0 };
        ranges.push(Range::StartLength { begin: address, length: u64::from(info.size) });
        let entry = dwarf.unit.add(root, gimli::DW_TAG_subprogram);
        let name = info.name.clone().unwrap_or_else(|| format!("f{i}"));
        dwarf.unit.get_mut(entry).set(gimli::DW_AT_name, AttributeValue::String(name.into_bytes()));
        dwarf.unit.get_mut(entry).set(gimli::DW_AT_low_pc, AttributeValue::Address(address));
        dwarf.unit.get_mut(entry).set(gimli::DW_AT_high_pc, AttributeValue::Udata(u64::from(info.size)));
        let mut frame = Expression::new();
        frame.op(gimli::DW_OP_call_frame_cfa);
        dwarf.unit.get_mut(entry).set(gimli::DW_AT_frame_base, AttributeValue::Exprloc(frame));
        for v in &info.vars {
            let key = (v.decl.type_name.clone(), v.decl.size, v.decl.encoding);
            let ty = *types.entry(key).or_insert_with(|| {
                let t = dwarf.unit.add(root, gimli::DW_TAG_base_type);
                let die = dwarf.unit.get_mut(t);
                die.set(gimli::DW_AT_name, AttributeValue::String(v.decl.type_name.clone().into_bytes()));
                die.set(gimli::DW_AT_byte_size, AttributeValue::Udata(u64::from(v.decl.size)));
                die.set(gimli::DW_AT_encoding, AttributeValue::Encoding(gimli::DwAte(v.decl.encoding as u8)));
                t
            });
            let locations: Vec<Location> = v
                .ranges
                .iter()
                .filter(|r| r.0 < r.1 && r.1 <= info.size)
                .map(|&(begin, end, loc)| {
                    let mut expr = Expression::new();
                    match loc {
                        VarLoc::Reg(r) => expr.op_reg(gimli::Register(r)),
                        VarLoc::Cfa(o) => expr.op_fbreg(o),
                    }
                    Location::StartEnd {
                        begin: Address::Symbol { symbol, addend: i64::from(begin) },
                        end: Address::Symbol { symbol, addend: i64::from(end) },
                        data: expr,
                    }
                })
                .collect();
            let tag = if v.decl.param { gimli::DW_TAG_formal_parameter } else { gimli::DW_TAG_variable };
            let var = dwarf.unit.add(entry, tag);
            dwarf.unit.get_mut(var).set(gimli::DW_AT_name, AttributeValue::String(v.decl.name.clone().into_bytes()));
            dwarf.unit.get_mut(var).set(gimli::DW_AT_type, AttributeValue::UnitRef(ty));
            if !locations.is_empty() {
                let list = dwarf.unit.locations.add(LocationList(locations));
                dwarf.unit.get_mut(var).set(gimli::DW_AT_location, AttributeValue::LocationListRef(list));
            }
        }
        let mut end = None;
        for r in &info.ranges {
            let Some(file) = files.get(&r.file) else { continue };
            // A gap or an unknown place must not inherit the previous row; a sequence per run also gives each function an end.
            if end != Some(r.off) {
                if let Some(end) = end {
                    lines.end_sequence(u64::from(end));
                }
                lines.begin_sequence(Some(address));
            }
            lines.row().address_offset = u64::from(r.off);
            lines.row().file = *file;
            lines.row().line = u64::from(r.line);
            lines.row().column = u64::from(r.column);
            lines.generate_row();
            end = Some(r.end);
        }
        if let Some(end) = end {
            lines.end_sequence(u64::from(end));
        }
    }
    if ranges.is_empty() {
        return Ok(());
    }
    let list = dwarf.unit.ranges.add(RangeList(ranges));
    dwarf.unit.get_mut(root).set(gimli::DW_AT_ranges, AttributeValue::RangeListRef(list));
    dwarf.unit.line_program = lines;
    let mut sections = Sections::new(Section { bytes: EndianVec::new(RunTimeEndian::Little), relocations: Vec::new() });
    dwarf.write(&mut sections).map_err(|e| format!("DWARF: {e}"))?;
    let format = product.object.format();
    let mut sec_ids = HashMap::new();
    // Mach-O DWARF sections refer to one another by plain offsets, with no relocation (llvm-mc writes them
    // so), and dsymutil rewrites them; only addresses of code carry relocations.
    if format == BinaryFormat::MachO {
        sections.for_each_mut(|_, s| -> Result<(), String> {
            let mut kept = Vec::new();
            for r in std::mem::take(&mut s.relocations) {
                if !matches!(r.target, RelocationTarget::Section(_)) {
                    kept.push(r);
                    continue;
                }
                let at = r.offset as usize;
                let bytes = s.bytes.slice().get(at..at + r.size as usize).ok_or("DWARF offset is outside its section")?;
                let mut value = [0u8; 8];
                value[..bytes.len()].copy_from_slice(bytes);
                let value = u64::from_le_bytes(value).wrapping_add(r.addend as u64);
                gimli::write::Writer::write_at(&mut s.bytes, at, &value.to_le_bytes()[..r.size as usize]).map_err(|e| format!("DWARF: {e}"))?;
            }
            s.relocations = kept;
            Ok(())
        })?;
    }
    sections.for_each(|id, s| -> Result<(), String> {
        if s.bytes.slice().is_empty() {
            return Ok(());
        }
        let name = if format == BinaryFormat::MachO { id.name().replacen('.', "__", 1) } else { id.name().to_string() };
        let segment = product.object.segment_name(StandardSegment::Debug).to_vec();
        let section = product.object.add_section(segment, name.into_bytes(), SectionKind::Debug);
        product.object.section_mut(section).set_data(s.bytes.slice().to_vec(), 1);
        sec_ids.insert(id, section);
        Ok(())
    })?;
    let bases = if format == BinaryFormat::MachO { section_addresses(product, &symbols)? } else { HashMap::new() };
    sections.for_each(|id, s| -> Result<(), String> {
        let Some(&section) = sec_ids.get(&id) else { return Ok(()) };
        for r in &s.relocations {
            let (symbol, kind, addend) = match r.target {
                RelocationTarget::Symbol(i) if format == BinaryFormat::MachO => {
                    // A Mach-O object holds the object-relative address of a function (its section's address
                    // plus its offset) with a relocation against the section, as llvm-mc writes it, which is
                    // what dsymutil matches against the debug map.
                    let function = product.object.symbol(symbols[i]);
                    let sec = function.section.id().ok_or("DWARF function is not defined in a section")?;
                    let base = *bases.get(&sec).ok_or("DWARF function section has no address")?;
                    let offset = i64::try_from(function.value + base).ok().and_then(|v| v.checked_add(r.addend)).ok_or("DWARF function offset is too large")?;
                    (product.object.section_symbol(sec), RelocationKind::Absolute, offset)
                }
                RelocationTarget::Symbol(i) => (symbols[i], RelocationKind::Absolute, r.addend),
                RelocationTarget::Section(sid) => {
                    let target = sec_ids.get(&sid).ok_or_else(|| format!("DWARF missing {}", sid.name()))?;
                    let kind = if format == BinaryFormat::Coff { RelocationKind::SectionOffset } else { RelocationKind::Absolute };
                    (product.object.section_symbol(*target), kind, r.addend)
                }
            };
            product
                .object
                .add_relocation(
                    section,
                    Relocation {
                        offset: r.offset as u64,
                        symbol,
                        addend,
                        flags: RelocationFlags::Generic { kind, encoding: RelocationEncoding::Generic, size: r.size * 8 },
                    },
                )
                .map_err(|e| format!("DWARF relocation: {e}"))?;
        }
        Ok(())
    })
}

/// Writes `.xdata` (each function's unwind record) and `.pdata` (the table mapping code ranges to records) for COFF
/// objects, with image-relative relocations the linker resolves.
fn emit_unwind(c: &Cl, product: &mut ObjectProduct, ids: &[FuncId]) -> Result<(), String> {
    if product.object.format() != BinaryFormat::Coff {
        return Ok(());
    }
    let arm64 = matches!(c.isa.triple().architecture, target_lexicon::Architecture::Aarch64(_));
    let infos = c.run.info.lock().unwrap();
    let (mut xdata, mut pdata) = (Vec::new(), Vec::new());
    let mut entries = Vec::new();
    for (i, info) in infos.iter().enumerate() {
        let Some(bytes) = info.unwind.as_ref().filter(|_| info.defined) else { continue };
        while xdata.len() % 4 != 0 {
            xdata.push(0);
        }
        entries.push((i, xdata.len() as i64, u64::from(info.size)));
        xdata.extend_from_slice(bytes);
    }
    if entries.is_empty() {
        return Ok(());
    }
    let stride = if arm64 { 8 } else { 12 };
    pdata.resize(entries.len() * stride, 0);
    let x = product.object.add_section(Vec::new(), b".xdata".to_vec(), SectionKind::ReadOnlyData);
    product.object.section_mut(x).set_data(xdata, 4);
    let p = product.object.add_section(Vec::new(), b".pdata".to_vec(), SectionKind::ReadOnlyData);
    product.object.section_mut(p).set_data(pdata, 4);
    let x_symbol = product.object.section_symbol(x);
    let image_offset = |size| RelocationFlags::Generic { kind: RelocationKind::ImageOffset, encoding: RelocationEncoding::Generic, size };
    for (n, &(i, at, size)) in entries.iter().enumerate() {
        let row = (n * stride) as u64;
        let function = product.function_symbol(ids[i]);
        let mut relocs = vec![(row, function, 0, image_offset(32))];
        if arm64 {
            relocs.push((row + 4, x_symbol, at, image_offset(32)));
        } else {
            relocs.push((row + 4, function, size as i64, image_offset(32)));
            relocs.push((row + 8, x_symbol, at, image_offset(32)));
        }
        for (offset, symbol, addend, flags) in relocs {
            product.object.add_relocation(p, Relocation { offset, symbol, addend, flags }).map_err(|e| format!("unwind relocation: {e}"))?;
        }
    }
    Ok(())
}

fn finish(c: &mut Cl) -> Result<Vec<u8>, String> {
    let ids: Vec<FuncId> = c.decls.read().unwrap().funcs.iter().map(|f| f.id).collect();
    let data_ids: Vec<DataId> = c.decls.read().unwrap().datas.iter().map(|d| d.id).collect();
    if unsafe { cl_wait(c) } != 0 {
        return Err(c.err.to_string_lossy().into_owned());
    }
    let mut module = c.module.take().ok_or("finished twice")?;
    for (i, done) in c.run.done.lock().unwrap().iter_mut().enumerate() {
        if let Some(done) = done.take() {
            module.define_function_bytes(ids[i], done.align, &done.code, &done.relocs).map_err(|e| format!("{e:?}"))?;
        }
    }
    let mut product = module.finish();
    for (i, id) in ids.iter().enumerate() {
        let info = &mut c.run.info.lock().unwrap()[i];
        if info.defined {
            info.offset = product.object.symbol(product.function_symbol(*id)).value;
        }
    }
    for r in &c.late {
        let dsym = product.object.symbol(product.data_symbol(data_ids[r.data as usize]));
        let SymbolSection::Section(section) = dsym.section else { return Err("a data object with relocations has no section".into()) };
        let offset = dsym.value + u64::from(r.offset);
        let target = product.function_symbol(ids[r.func as usize]);
        product
            .object
            .add_relocation(
                section,
                Relocation {
                    offset,
                    symbol: target,
                    addend: r.addend,
                    flags: RelocationFlags::Generic { kind: RelocationKind::Absolute, encoding: RelocationEncoding::Generic, size: 64 },
                },
            )
            .map_err(|e| format!("relocation: {e}"))?;
    }
    emit_dwarf(c, &mut product, &ids)?;
    emit_unwind(c, &mut product, &ids)?;
    product.emit().map_err(|e| e.to_string())
}

/// Writes the object. The bytes stay valid until `cl_free`. After this the module accepts no more input.
/// Null on failure, with the message in `cl_error`.
#[no_mangle]
pub unsafe extern "C" fn cl_finish(c: *mut Cl, len: *mut usize) -> *const u8 {
    let c = cl(c);
    match finish(c) {
        Ok(bytes) => {
            c.object = bytes;
            *len = c.object.len();
            c.object.as_ptr()
        }
        Err(e) => {
            c.err = cerr(e);
            *len = 0;
            std::ptr::null()
        }
    }
}

/// Offset of `func` in the object's text section and its size in bytes; valid after `cl_finish`.
#[no_mangle]
pub unsafe extern "C" fn cl_func_info(c: *mut Cl, func: Func, offset: *mut u64, size: *mut u32) {
    let info = cl(c).run.info.lock().unwrap();
    let i = &info[func.0 as usize];
    *offset = i.offset;
    *size = i.size;
}

/// The code ranges of `func` with their source places, sorted by offset: for example to build tables mapping code addresses to places.
#[no_mangle]
pub unsafe extern "C" fn cl_func_ranges(c: *mut Cl, func: Func, n: *mut u32) -> *const ClCodeRange {
    let info = cl(c).run.info.lock().unwrap();
    let i = &info[func.0 as usize];
    *n = i.ranges.len() as u32;
    i.ranges.as_ptr()
}
