/* Loads the library by path (the way the compiler does), builds a module that uses every family of the API, with
 * functions compiled on several threads, and writes the object to argv[2]. smoke/main.c is linked with that object and
 * runs the functions. */
#define CL_DYNAMIC
#define CL_DEFINE_API
#include "../include/cranelift.h"
#include <stdio.h>
#include <stdlib.h>
#ifdef _WIN32
#include <windows.h>
#define LOAD(p) ((void *)LoadLibraryA(p))
static void *resolve(void *h, const char *n) { return (void *)GetProcAddress((HMODULE)h, n); }
typedef HANDLE thread_t;
typedef CRITICAL_SECTION lock_t;
#define LOCK_INIT(l) InitializeCriticalSection(l)
#define LOCK(l) EnterCriticalSection(l)
#define UNLOCK(l) LeaveCriticalSection(l)
static thread_t spawn(DWORD(WINAPI *fn)(void *), void *arg) { return CreateThread(NULL, 0, fn, arg, 0, NULL); }
static void join(thread_t t) { WaitForSingleObject(t, INFINITE); CloseHandle(t); }
#define THREAD_FN(name) static DWORD WINAPI name(void *arg)
#define THREAD_RET return 0
#else
#include <dlfcn.h>
#include <pthread.h>
#define LOAD(p) dlopen(p, RTLD_NOW | RTLD_LOCAL)
static void *resolve(void *h, const char *n) { return dlsym(h, n); }
typedef pthread_t thread_t;
typedef pthread_mutex_t lock_t;
#define LOCK_INIT(l) pthread_mutex_init(l, NULL)
#define LOCK(l) pthread_mutex_lock(l)
#define UNLOCK(l) pthread_mutex_unlock(l)
static thread_t spawn(void *(*fn)(void *), void *arg) { pthread_t t; pthread_create(&t, NULL, fn, arg); return t; }
static void join(thread_t t) { pthread_join(t, NULL); }
#define THREAD_FN(name) static void *name(void *arg)
#define THREAD_RET return NULL
#endif

static cl *c;
static void check(int bad, const char *what) {
  if (bad) { fprintf(stderr, "%s failed: %s\n", what, cl_error(c)); exit(1); }
}
static cl_sig sig(cl_type ret, int nparams, cl_type param) {
  cl_sig s = cl_sig_new(c);
  for (int i = 0; i < nparams; i++) cl_sig_param(c, s, param, CL_EXT_NONE);
  cl_sig_ret(c, s, ret, CL_EXT_NONE);
  return s;
}
/* Queues what f built; the module's workers compile it. */
static void finish_fn(cl_fn *f, const char *what) {
  if (cl_fn_end(f)) { fprintf(stderr, "%s: %s\n", what, cl_fn_error(f)); exit(1); }
}

enum { WORKERS = 4, PER_WORKER = 16 };
static cl_func par_funcs[WORKERS * PER_WORKER];
static cl_sig par_sig;
/* pf_i(x) = x + i, built and compiled on a thread each. */
THREAD_FN(worker) {
  int w = (int)(size_t)arg;
  cl_fn *f = cl_fn_new(c);
  for (int k = 0; k < PER_WORKER; k++) {
    int i = w * PER_WORKER + k;
    cl_fn_begin(f, par_funcs[i]);
    cl_block b = cl_entry(f);
    cl_srcloc(f, 0, 100 + i, 1);
    cl_ret1(f, cl_iadd(f, cl_block_param(f, b, 0), cl_i64const(f, i)));
    cl_seal_all(f);
    finish_fn(f, "parallel function");
  }
  cl_fn_free(f);
  THREAD_RET;
}

int main(int argc, char **argv) {
  if (argc < 3) return 2;
  void *h = LOAD(argv[1]);
  if (!h) { fprintf(stderr, "cannot load %s\n", argv[1]); return 1; }
  const char *missing = cl_bind(resolve, h);
  if (missing) { fprintf(stderr, "missing %s\n", missing); return 1; }
  const char *k[] = {"opt_level", "regalloc_algorithm"}, *v[] = {"none", "single_pass"};
  c = cl_new(NULL, 2, k, v);
  if (!c) { fprintf(stderr, "new: %s\n", cl_error(NULL)); return 1; }

  cl_sig s_ii = sig(CL_I64, 2, CL_I64), s_i = sig(CL_I64, 1, CL_I64), s_f = sig(CL_I64, 1, CL_F64);
  par_sig = s_i;
  cl_func f_add = cl_declare_func(c, "t_add", s_ii, CL_EXPORT);      /* a + b */
  cl_func f_sum = cl_declare_func(c, "t_sum", s_i, CL_EXPORT);       /* 0 + 1 + ... + n: a loop with block parameters */
  cl_func f_chk = cl_declare_func(c, "t_checked", s_ii, CL_EXPORT);  /* a + b, or -1 on signed overflow (a cold block) */
  cl_func f_fl = cl_declare_func(c, "t_float", s_f, CL_EXPORT);      /* min(1000, (i64)sqrt(a * a + 1)) */
  cl_func f_mem = cl_declare_func(c, "t_mem", s_i, CL_EXPORT);       /* slot, narrow stores and loads, data, atomic, switch, call */
  cl_func f_ind = cl_declare_func(c, "t_indirect", s_ii, CL_EXPORT); /* t_add through a pointer held in data */
  cl_func f_bulk = cl_declare_func(c, "t_bulk", s_i, CL_EXPORT);    /* bulk memory: fill, copy (small and large), move */
  for (int i = 0; i < WORKERS * PER_WORKER; i++) {
    char name[32];
    snprintf(name, sizeof name, "pf_%d", i);
    par_funcs[i] = cl_declare_func(c, name, par_sig, CL_EXPORT);
    cl_func_name(c, par_funcs[i], name);
  }
  cl_data d_tab = cl_declare_data(c, "t_table", CL_LOCAL, 0, 0);
  cl_data d_ptr = cl_declare_data(c, "t_fnptr", CL_LOCAL, 1, 0);
  cl_data d_cnt = cl_declare_data(c, "t_counter", CL_EXPORT, 1, 0);
  cl_data d_pcs = cl_declare_data(c, "t_pcs", CL_EXPORT, 0, 0);
  cl_func_name(c, f_chk, "checked");
  cl_dwarf_dir(c, ".");
  cl_dwarf_file(c, 0, "smoke.src");

  uint8_t tab[16] = {10, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0};
  cl_define_data(c, d_tab, tab, sizeof tab, 8, NULL, 0);
  cl_reloc r = {0, 0, f_add.id, 0};
  cl_define_data(c, d_ptr, NULL, 8, 8, &r, 1);
  cl_define_data(c, d_cnt, NULL, 8, 8, NULL, 0);
  /* Two code addresses, the way the fault tables hold them: a function plus an offset into it. */
  cl_reloc pcs[3] = {{0, 0, f_add.id, 4}, {8, 0, f_sum.id, 0}, {16, 1, d_pcs.id, 8}};   /* and the table's own address */
  cl_define_data(c, d_pcs, NULL, 24, 8, pcs, 3);

  thread_t t[WORKERS];
  for (int w = 0; w < WORKERS; w++) t[w] = spawn(worker, (void *)(size_t)w);

  cl_fn *f = cl_fn_new(c);
  cl_block b;
  cl_fn_begin(f, f_add);
  b = cl_entry(f);
  cl_ret1(f, cl_iadd(f, cl_block_param(f, b, 0), cl_block_param(f, b, 1)));
  cl_seal_all(f);
  finish_fn(f, "t_add");

  cl_fn_begin(f, f_sum);
  b = cl_entry(f);
  cl_val n = cl_block_param(f, b, 0);
  cl_block head = cl_block_new(f), body = cl_block_new(f), done = cl_block_new(f);
  cl_val i = cl_block_param_add(f, head, CL_I64), acc = cl_block_param_add(f, head, CL_I64);
  cl_val zero = cl_i64const(f, 0), one = cl_i64const(f, 1);
  cl_jump(f, head, (cl_val[]){zero, zero}, 2);
  cl_enter(f, head);
  cl_br(f, cl_icmp(f, CL_SLE, i, n), body, done);
  cl_enter(f, body);
  cl_jump(f, head, (cl_val[]){cl_iadd(f, i, one), cl_iadd(f, acc, i)}, 2);
  cl_enter(f, done);
  cl_ret1(f, acc);
  cl_seal_all(f);
  finish_fn(f, "t_sum");

  cl_fn_begin(f, f_chk);
  b = cl_entry(f);
  cl_srcloc(f, 0, 42, 3);
  cl_ovf_result sum = cl_ovf(f, CL_SADD_OVF, cl_block_param(f, b, 0), cl_block_param(f, b, 1));
  cl_block ok = cl_block_new(f), overflow = cl_block_new(f);
  cl_br(f, sum.flag, overflow, ok);
  cl_cold(f, overflow);
  cl_enter(f, overflow);
  cl_srcloc(f, 0, 43, 3);
  cl_ret1(f, cl_i64const(f, -1));
  cl_enter(f, ok);
  cl_srcloc(f, 0, 44, 4);
  cl_ret1(f, sum.result);
  cl_seal_all(f);
  finish_fn(f, "t_checked");

  cl_fn_begin(f, f_fl);
  b = cl_entry(f);
  cl_val x = cl_block_param(f, b, 0);
  cl_val root = cl_sqrt(f, cl_fadd(f, cl_fmul(f, x, x), cl_f64const(f, 1.0)));
  cl_val cut = cl_cast(f, CL_FCVT_TO_SINT_SAT, CL_I64, root);
  cl_val big = cl_fcmp(f, CL_FGT, root, cl_f64const(f, 1000.0));
  cl_ret1(f, cl_select(f, big, cl_i64const(f, 1000), cut));
  cl_seal_all(f);
  finish_fn(f, "t_float");

  cl_fn_begin(f, f_mem);
  b = cl_entry(f);
  x = cl_block_param(f, b, 0);
  cl_val sa = cl_stack_addr(f, cl_stack_slot(f, 16, 3), 0);
  cl_store_trunc(f, 8, 0, cl_i64const(f, 0x1ff), sa, 0);
  cl_store_trunc(f, 16, 0, x, sa, 2);
  cl_val ta = cl_data_addr(f, d_tab);
  cl_val tv = cl_iadd(f, cl_load_ext(f, 8, 0, CL_I64, 0, sa, 0),           /* 0xff */
                            cl_load_ext(f, 16, 1, CL_I64, 0, sa, 2));           /* x's low 16 bits, signed */
  tv = cl_iadd(f, tv, cl_load_ext(f, 32, 1, CL_I64, 0, ta, 8));               /* -1 */
  tv = cl_iadd(f, tv, cl_load(f, CL_I64, CL_NOTRAP_ALIGNED, ta, 0));              /* 10 */
  cl_atomic_rmw(f, CL_RMW_ADD, CL_I64, 0, cl_data_addr(f, d_cnt), cl_i64const(f, 5));
  cl_block two = cl_block_new(f), other = cl_block_new(f);
  cl_switch(f, x, other, (uint64_t[]){2}, (cl_block[]){two}, 1);
  cl_enter(f, two);
  cl_ret1(f, cl_call1(f, f_add, (cl_val[]){tv, cl_i64const(f, 1000)}, 2));
  cl_enter(f, other);
  cl_ret1(f, tv);
  cl_seal_all(f);
  finish_fn(f, "t_mem");

  cl_fn_begin(f, f_ind);
  b = cl_entry(f);
  cl_val fp = cl_load(f, CL_I64, 0, cl_data_addr(f, d_ptr), 0);
  cl_ret1(f, cl_call_indirect1(f, s_ii, fp, (cl_val[]){cl_block_param(f, b, 0), cl_block_param(f, b, 1)}, 2));
  cl_seal_all(f);
  finish_fn(f, "t_indirect");
  cl_fn_begin(f, f_bulk);
  b = cl_entry(f);
  x = cl_block_param(f, b, 0);
  cl_val from = cl_stack_addr(f, cl_stack_slot(f, 512, 3), 0);
  cl_val to = cl_stack_addr(f, cl_stack_slot(f, 512, 3), 0);
  cl_fill(f, from, 0, 512);                   /* large: a call of memset */
  cl_store(f, CL_NOTRAP_ALIGNED, x, from, 8);
  cl_store(f, CL_NOTRAP_ALIGNED, x, from, 300);
  cl_copy(f, to, from, 512);                  /* large: a call of memcpy */
  cl_copy(f, from, to, 16);                   /* small: loads and stores */
  cl_copy_dyn(f, to, from, cl_i64const(f, 8)); /* memmove */
  cl_ret1(f, cl_iadd(f, cl_load(f, CL_I64, CL_NOTRAP_ALIGNED, to, 8), cl_load(f, CL_I64, CL_NOTRAP_ALIGNED, to, 300)));
  cl_seal_all(f);
  finish_fn(f, "t_bulk");
  cl_fn_free(f);

  for (int w = 0; w < WORKERS; w++) join(t[w]);

  /* ranges exist once the workers are done */
  check(cl_wait(c), "compile");
  /* the places recorded for t_checked: line 42 (column 3) comes first, line 44 sits after the cold block */
  uint32_t nr = 0;
  const cl_code_range *rs = cl_func_ranges(c, f_chk, &nr);
  int saw42 = 0, saw43 = 0, saw44 = 0;
  for (uint32_t j = 0; j < nr; j++) {
    if (rs[j].file == 0 && rs[j].line == 42 && rs[j].column == 3) saw42 = 1;
    if (rs[j].file == 0 && rs[j].line == 43) saw43 = 1;
    if (rs[j].file == 0 && rs[j].line == 44 && rs[j].column == 4) saw44 = 1;
  }
  size_t len = 0;
  const uint8_t *o = cl_finish(c, &len);
  check(!o, "finish");
  FILE *fh = fopen(argv[2], "wb");
  if (!fh || fwrite(o, 1, len, fh) != len) { fprintf(stderr, "cannot write %s\n", argv[2]); return 1; }
  fclose(fh);
  int ok_places = saw42 && saw43 && saw44;
  printf("object %zu bytes, %u ranges in t_checked, places %s\n", len, nr, ok_places ? "ok" : "MISSING");
  cl_free(c);
  return ok_places ? 0 : 1;
}
