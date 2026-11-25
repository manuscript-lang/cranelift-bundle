/* Loads the library by path (the way the compiler does), builds a module that uses every family of the API, with
 * functions compiled on several threads, and writes the object to argv[2]. smoke/main.c is linked with that object and
 * runs the functions. */
#define MS_CL_DYNAMIC
#define MS_CL_DEFINE_API
#include "../include/ms_cranelift.h"
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

static ms_cl *c;
static void check(int bad, const char *what) {
  if (bad) { fprintf(stderr, "%s failed: %s\n", what, ms_cl_error(c)); exit(1); }
}
static ms_cl_sig sig(ms_cl_type ret, int nparams, ms_cl_type param) {
  ms_cl_sig s = ms_cl_sig_new(c);
  for (int i = 0; i < nparams; i++) ms_cl_sig_param(c, s, param, MS_CL_EXT_NONE);
  ms_cl_sig_ret(c, s, ret, MS_CL_EXT_NONE);
  return s;
}
/* Queues what f built; the module's workers compile it. */
static void finish_fn(ms_cl_fn *f, const char *what) {
  if (ms_cl_fn_end(f)) { fprintf(stderr, "%s: %s\n", what, ms_cl_fn_error(f)); exit(1); }
}

enum { WORKERS = 4, PER_WORKER = 16 };
static ms_cl_func par_funcs[WORKERS * PER_WORKER];
static ms_cl_sig par_sig;
/* pf_i(x) = x + i, built and compiled on a thread each. */
THREAD_FN(worker) {
  int w = (int)(size_t)arg;
  ms_cl_fn *f = ms_cl_fn_new(c);
  for (int k = 0; k < PER_WORKER; k++) {
    int i = w * PER_WORKER + k;
    ms_cl_fn_begin(f, par_funcs[i]);
    ms_cl_block b = ms_cl_entry(f);
    ms_cl_srcloc(f, 0, 100 + i, 1);
    ms_cl_ret1(f, ms_cl_iadd(f, ms_cl_block_param(f, b, 0), ms_cl_i64const(f, i)));
    ms_cl_seal_all(f);
    finish_fn(f, "parallel function");
  }
  ms_cl_fn_free(f);
  THREAD_RET;
}

int main(int argc, char **argv) {
  if (argc < 3) return 2;
  void *h = LOAD(argv[1]);
  if (!h) { fprintf(stderr, "cannot load %s\n", argv[1]); return 1; }
  const char *missing = ms_cl_bind(resolve, h);
  if (missing) { fprintf(stderr, "missing %s\n", missing); return 1; }
  const char *k[] = {"opt_level", "regalloc_algorithm"}, *v[] = {"none", "single_pass"};
  c = ms_cl_new(NULL, 2, k, v);
  if (!c) { fprintf(stderr, "new: %s\n", ms_cl_error(NULL)); return 1; }

  ms_cl_sig s_ii = sig(MS_CL_I64, 2, MS_CL_I64), s_i = sig(MS_CL_I64, 1, MS_CL_I64), s_f = sig(MS_CL_I64, 1, MS_CL_F64);
  par_sig = s_i;
  ms_cl_func f_add = ms_cl_declare_func(c, "ms_add", s_ii, MS_CL_EXPORT);      /* a + b */
  ms_cl_func f_sum = ms_cl_declare_func(c, "ms_sum", s_i, MS_CL_EXPORT);       /* 0 + 1 + ... + n: a loop with block parameters */
  ms_cl_func f_chk = ms_cl_declare_func(c, "ms_checked", s_ii, MS_CL_EXPORT);  /* a + b, or -1 on signed overflow (a cold block) */
  ms_cl_func f_fl = ms_cl_declare_func(c, "ms_float", s_f, MS_CL_EXPORT);      /* min(1000, (i64)sqrt(a * a + 1)) */
  ms_cl_func f_mem = ms_cl_declare_func(c, "ms_mem", s_i, MS_CL_EXPORT);       /* slot, narrow stores and loads, data, atomic, switch, call */
  ms_cl_func f_ind = ms_cl_declare_func(c, "ms_indirect", s_ii, MS_CL_EXPORT); /* ms_add through a pointer held in data */
  ms_cl_func f_bulk = ms_cl_declare_func(c, "ms_bulk", s_i, MS_CL_EXPORT);    /* bulk memory: fill, copy (small and large), move */
  for (int i = 0; i < WORKERS * PER_WORKER; i++) {
    char name[32];
    snprintf(name, sizeof name, "pf_%d", i);
    par_funcs[i] = ms_cl_declare_func(c, name, par_sig, MS_CL_EXPORT);
    ms_cl_func_name(c, par_funcs[i], name);
  }
  ms_cl_data d_tab = ms_cl_declare_data(c, "ms_table", MS_CL_LOCAL, 0, 0);
  ms_cl_data d_ptr = ms_cl_declare_data(c, "ms_fnptr", MS_CL_LOCAL, 1, 0);
  ms_cl_data d_cnt = ms_cl_declare_data(c, "ms_counter", MS_CL_EXPORT, 1, 0);
  ms_cl_data d_pcs = ms_cl_declare_data(c, "ms_pcs", MS_CL_EXPORT, 0, 0);
  ms_cl_func_name(c, f_chk, "checked");
  ms_cl_dwarf_dir(c, ".");
  ms_cl_dwarf_file(c, 0, "smoke.ms");

  uint8_t tab[16] = {10, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0};
  ms_cl_define_data(c, d_tab, tab, sizeof tab, 8, NULL, 0);
  ms_cl_reloc r = {0, 0, f_add.id, 0};
  ms_cl_define_data(c, d_ptr, NULL, 8, 8, &r, 1);
  ms_cl_define_data(c, d_cnt, NULL, 8, 8, NULL, 0);
  /* Two code addresses, the way the fault tables hold them: a function plus an offset into it. */
  ms_cl_reloc pcs[3] = {{0, 0, f_add.id, 4}, {8, 0, f_sum.id, 0}, {16, 1, d_pcs.id, 8}};   /* and the table's own address */
  ms_cl_define_data(c, d_pcs, NULL, 24, 8, pcs, 3);

  thread_t t[WORKERS];
  for (int w = 0; w < WORKERS; w++) t[w] = spawn(worker, (void *)(size_t)w);

  ms_cl_fn *f = ms_cl_fn_new(c);
  ms_cl_block b;
  ms_cl_fn_begin(f, f_add);
  b = ms_cl_entry(f);
  ms_cl_ret1(f, ms_cl_iadd(f, ms_cl_block_param(f, b, 0), ms_cl_block_param(f, b, 1)));
  ms_cl_seal_all(f);
  finish_fn(f, "ms_add");

  ms_cl_fn_begin(f, f_sum);
  b = ms_cl_entry(f);
  ms_cl_val n = ms_cl_block_param(f, b, 0);
  ms_cl_block head = ms_cl_block_new(f), body = ms_cl_block_new(f), done = ms_cl_block_new(f);
  ms_cl_val i = ms_cl_block_param_add(f, head, MS_CL_I64), acc = ms_cl_block_param_add(f, head, MS_CL_I64);
  ms_cl_val zero = ms_cl_i64const(f, 0), one = ms_cl_i64const(f, 1);
  ms_cl_jump(f, head, (ms_cl_val[]){zero, zero}, 2);
  ms_cl_enter(f, head);
  ms_cl_br(f, ms_cl_icmp(f, MS_CL_SLE, i, n), body, done);
  ms_cl_enter(f, body);
  ms_cl_jump(f, head, (ms_cl_val[]){ms_cl_iadd(f, i, one), ms_cl_iadd(f, acc, i)}, 2);
  ms_cl_enter(f, done);
  ms_cl_ret1(f, acc);
  ms_cl_seal_all(f);
  finish_fn(f, "ms_sum");

  ms_cl_fn_begin(f, f_chk);
  b = ms_cl_entry(f);
  ms_cl_srcloc(f, 0, 42, 3);
  ms_cl_ovf_result sum = ms_cl_ovf(f, MS_CL_SADD_OVF, ms_cl_block_param(f, b, 0), ms_cl_block_param(f, b, 1));
  ms_cl_block ok = ms_cl_block_new(f), overflow = ms_cl_block_new(f);
  ms_cl_br(f, sum.flag, overflow, ok);
  ms_cl_cold(f, overflow);
  ms_cl_enter(f, overflow);
  ms_cl_srcloc(f, 0, 43, 3);
  ms_cl_ret1(f, ms_cl_i64const(f, -1));
  ms_cl_enter(f, ok);
  ms_cl_srcloc(f, 0, 44, 4);
  ms_cl_ret1(f, sum.result);
  ms_cl_seal_all(f);
  finish_fn(f, "ms_checked");

  ms_cl_fn_begin(f, f_fl);
  b = ms_cl_entry(f);
  ms_cl_val x = ms_cl_block_param(f, b, 0);
  ms_cl_val root = ms_cl_sqrt(f, ms_cl_fadd(f, ms_cl_fmul(f, x, x), ms_cl_f64const(f, 1.0)));
  ms_cl_val cut = ms_cl_cast(f, MS_CL_FCVT_TO_SINT_SAT, MS_CL_I64, root);
  ms_cl_val big = ms_cl_fcmp(f, MS_CL_FGT, root, ms_cl_f64const(f, 1000.0));
  ms_cl_ret1(f, ms_cl_select(f, big, ms_cl_i64const(f, 1000), cut));
  ms_cl_seal_all(f);
  finish_fn(f, "ms_float");

  ms_cl_fn_begin(f, f_mem);
  b = ms_cl_entry(f);
  x = ms_cl_block_param(f, b, 0);
  ms_cl_val sa = ms_cl_stack_addr(f, ms_cl_stack_slot(f, 16, 3), 0);
  ms_cl_store_trunc(f, 8, 0, ms_cl_i64const(f, 0x1ff), sa, 0);
  ms_cl_store_trunc(f, 16, 0, x, sa, 2);
  ms_cl_val ta = ms_cl_data_addr(f, d_tab);
  ms_cl_val tv = ms_cl_iadd(f, ms_cl_load_ext(f, 8, 0, MS_CL_I64, 0, sa, 0),           /* 0xff */
                            ms_cl_load_ext(f, 16, 1, MS_CL_I64, 0, sa, 2));           /* x's low 16 bits, signed */
  tv = ms_cl_iadd(f, tv, ms_cl_load_ext(f, 32, 1, MS_CL_I64, 0, ta, 8));               /* -1 */
  tv = ms_cl_iadd(f, tv, ms_cl_load(f, MS_CL_I64, MS_CL_TRUSTED, ta, 0));              /* 10 */
  ms_cl_atomic_rmw(f, MS_CL_RMW_ADD, MS_CL_I64, 0, ms_cl_data_addr(f, d_cnt), ms_cl_i64const(f, 5));
  ms_cl_block two = ms_cl_block_new(f), other = ms_cl_block_new(f);
  ms_cl_switch(f, x, other, (uint64_t[]){2}, (ms_cl_block[]){two}, 1);
  ms_cl_enter(f, two);
  ms_cl_ret1(f, ms_cl_call1(f, f_add, (ms_cl_val[]){tv, ms_cl_i64const(f, 1000)}, 2));
  ms_cl_enter(f, other);
  ms_cl_ret1(f, tv);
  ms_cl_seal_all(f);
  finish_fn(f, "ms_mem");

  ms_cl_fn_begin(f, f_ind);
  b = ms_cl_entry(f);
  ms_cl_val fp = ms_cl_load(f, MS_CL_I64, 0, ms_cl_data_addr(f, d_ptr), 0);
  ms_cl_ret1(f, ms_cl_call_indirect1(f, s_ii, fp, (ms_cl_val[]){ms_cl_block_param(f, b, 0), ms_cl_block_param(f, b, 1)}, 2));
  ms_cl_seal_all(f);
  finish_fn(f, "ms_indirect");
  ms_cl_fn_begin(f, f_bulk);
  b = ms_cl_entry(f);
  x = ms_cl_block_param(f, b, 0);
  ms_cl_val from = ms_cl_stack_addr(f, ms_cl_stack_slot(f, 512, 3), 0);
  ms_cl_val to = ms_cl_stack_addr(f, ms_cl_stack_slot(f, 512, 3), 0);
  ms_cl_fill(f, from, 0, 512);                   /* large: a call of memset */
  ms_cl_store(f, MS_CL_TRUSTED, x, from, 8);
  ms_cl_store(f, MS_CL_TRUSTED, x, from, 300);
  ms_cl_copy(f, to, from, 512);                  /* large: a call of memcpy */
  ms_cl_copy(f, from, to, 16);                   /* small: loads and stores */
  ms_cl_copy_dyn(f, to, from, ms_cl_i64const(f, 8)); /* memmove */
  ms_cl_ret1(f, ms_cl_iadd(f, ms_cl_load(f, MS_CL_I64, MS_CL_TRUSTED, to, 8), ms_cl_load(f, MS_CL_I64, MS_CL_TRUSTED, to, 300)));
  ms_cl_seal_all(f);
  finish_fn(f, "ms_bulk");
  ms_cl_fn_free(f);

  for (int w = 0; w < WORKERS; w++) join(t[w]);

  /* ranges exist once the workers are done */
  check(ms_cl_wait(c), "compile");
  /* the places recorded for ms_checked: line 42 (scope 3) comes first, line 44 sits after the cold block */
  uint32_t nr = 0;
  const ms_cl_code_range *rs = ms_cl_func_ranges(c, f_chk, &nr);
  int saw42 = 0, saw43 = 0, saw44 = 0;
  for (uint32_t j = 0; j < nr; j++) {
    if (rs[j].file == 0 && rs[j].line == 42 && rs[j].scope == 3) saw42 = 1;
    if (rs[j].file == 0 && rs[j].line == 43) saw43 = 1;
    if (rs[j].file == 0 && rs[j].line == 44 && rs[j].scope == 4) saw44 = 1;
  }
  size_t len = 0;
  const uint8_t *o = ms_cl_finish(c, &len);
  check(!o, "finish");
  FILE *fh = fopen(argv[2], "wb");
  if (!fh || fwrite(o, 1, len, fh) != len) { fprintf(stderr, "cannot write %s\n", argv[2]); return 1; }
  fclose(fh);
  int ok_places = saw42 && saw43 && saw44;
  printf("object %zu bytes, %u ranges in ms_checked, places %s\n", len, nr, ok_places ? "ok" : "MISSING");
  ms_cl_free(c);
  return ok_places ? 0 : 1;
}
