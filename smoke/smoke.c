/* Loads the library by path (the way the compiler does), builds a module that uses every family of the API and writes
 * the object to argv[2]. smoke/main.c is linked with that object and runs the functions. */
#define MS_CL_DYNAMIC
#define MS_CL_DEFINE_API
#include "../include/ms_cranelift.h"
#include <stdio.h>
#include <stdlib.h>
#ifdef _WIN32
#include <windows.h>
#define LOAD(p) ((void *)LoadLibraryA(p))
static void *resolve(void *h, const char *n) { return (void *)GetProcAddress((HMODULE)h, n); }
#else
#include <dlfcn.h>
#define LOAD(p) dlopen(p, RTLD_NOW | RTLD_LOCAL)
static void *resolve(void *h, const char *n) { return dlsym(h, n); }
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
  /* i64 ms_add(a, b) = a + b */
  ms_cl_func f_add = ms_cl_declare_func(c, "ms_add", s_ii, MS_CL_EXPORT);
  /* i64 ms_sum(n) = 0 + 1 + ... + n, a loop with block parameters */
  ms_cl_func f_sum = ms_cl_declare_func(c, "ms_sum", s_i, MS_CL_EXPORT);
  /* i64 ms_checked(a, b) = a + b, or -1 on signed overflow (a cold block) */
  ms_cl_func f_chk = ms_cl_declare_func(c, "ms_checked", s_ii, MS_CL_EXPORT);
  /* i64 ms_float(double a) = min(1000, (i64)sqrt(a * a + 1)) */
  ms_cl_func f_fl = ms_cl_declare_func(c, "ms_float", s_f, MS_CL_EXPORT);
  /* i64 ms_mem(x): stack slot, narrow stores and widening loads, data, an atomic, a switch and a call */
  ms_cl_func f_mem = ms_cl_declare_func(c, "ms_mem", s_i, MS_CL_EXPORT);
  /* i64 ms_indirect(a, b): calls ms_add through a pointer held in data */
  ms_cl_func f_ind = ms_cl_declare_func(c, "ms_indirect", s_ii, MS_CL_EXPORT);
  ms_cl_data d_tab = ms_cl_declare_data(c, "ms_table", MS_CL_LOCAL, 0, 0);
  ms_cl_data d_ptr = ms_cl_declare_data(c, "ms_fnptr", MS_CL_LOCAL, 1, 0);
  ms_cl_data d_cnt = ms_cl_declare_data(c, "ms_counter", MS_CL_EXPORT, 1, 0);

  uint8_t tab[16] = {10, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0};
  ms_cl_define_data(c, d_tab, tab, sizeof tab, 8, NULL, 0);
  ms_cl_reloc r = {0, 0, f_add.id, 0};
  ms_cl_define_data(c, d_ptr, NULL, 8, 8, &r, 1);
  ms_cl_define_data(c, d_cnt, NULL, 8, 8, NULL, 0);

  ms_cl_block b;
  ms_cl_func_begin(c, f_add);
  b = ms_cl_entry(c);
  ms_cl_ret1(c, ms_cl_iadd(c, ms_cl_block_param(c, b, 0), ms_cl_block_param(c, b, 1)));
  ms_cl_seal_all(c);
  check(ms_cl_func_end(c), "ms_add");

  ms_cl_func_begin(c, f_sum);
  b = ms_cl_entry(c);
  ms_cl_val n = ms_cl_block_param(c, b, 0);
  ms_cl_block head = ms_cl_block_new(c), body = ms_cl_block_new(c), done = ms_cl_block_new(c);
  ms_cl_val i = ms_cl_block_param_add(c, head, MS_CL_I64), acc = ms_cl_block_param_add(c, head, MS_CL_I64);
  ms_cl_val zero = ms_cl_i64const(c, 0), one = ms_cl_i64const(c, 1);
  ms_cl_jump(c, head, (ms_cl_val[]){zero, zero}, 2);
  ms_cl_enter(c, head);
  ms_cl_br(c, ms_cl_icmp(c, MS_CL_SLE, i, n), body, done);
  ms_cl_enter(c, body);
  ms_cl_jump(c, head, (ms_cl_val[]){ms_cl_iadd(c, i, one), ms_cl_iadd(c, acc, i)}, 2);
  ms_cl_enter(c, done);
  ms_cl_ret1(c, acc);
  ms_cl_seal_all(c);
  check(ms_cl_func_end(c), "ms_sum");

  ms_cl_func_begin(c, f_chk);
  b = ms_cl_entry(c);
  ms_cl_srcloc(c, 7);
  ms_cl_ovf_result sum = ms_cl_ovf(c, MS_CL_SADD_OVF, ms_cl_block_param(c, b, 0), ms_cl_block_param(c, b, 1));
  ms_cl_block ok = ms_cl_block_new(c), overflow = ms_cl_block_new(c);
  ms_cl_br(c, sum.flag, overflow, ok);
  ms_cl_cold(c, overflow);
  ms_cl_enter(c, overflow);
  ms_cl_ret1(c, ms_cl_i64const(c, -1));
  ms_cl_enter(c, ok);
  ms_cl_ret1(c, sum.result);
  ms_cl_seal_all(c);
  check(ms_cl_func_end(c), "ms_checked");

  ms_cl_func_begin(c, f_fl);
  b = ms_cl_entry(c);
  ms_cl_val x = ms_cl_block_param(c, b, 0);
  ms_cl_val root = ms_cl_sqrt(c, ms_cl_fadd(c, ms_cl_fmul(c, x, x), ms_cl_f64const(c, 1.0)));
  ms_cl_val cut = ms_cl_cast(c, MS_CL_FCVT_TO_SINT_SAT, MS_CL_I64, root);
  ms_cl_val big = ms_cl_fcmp(c, MS_CL_FGT, root, ms_cl_f64const(c, 1000.0));
  ms_cl_ret1(c, ms_cl_select(c, big, ms_cl_i64const(c, 1000), cut));
  ms_cl_seal_all(c);
  check(ms_cl_func_end(c), "ms_float");

  ms_cl_func_begin(c, f_mem);
  b = ms_cl_entry(c);
  x = ms_cl_block_param(c, b, 0);
  ms_cl_val sa = ms_cl_stack_addr(c, ms_cl_stack_slot(c, 16, 3), 0);
  ms_cl_store_trunc(c, 8, 0, ms_cl_i64const(c, 0x1ff), sa, 0);
  ms_cl_store_trunc(c, 16, 0, x, sa, 2);
  ms_cl_val ta = ms_cl_data_addr(c, d_tab);
  ms_cl_val t = ms_cl_iadd(c, ms_cl_load_ext(c, 8, 0, MS_CL_I64, 0, sa, 0),             /* 0xff */
                           ms_cl_load_ext(c, 16, 1, MS_CL_I64, 0, sa, 2));             /* x's low 16 bits, signed */
  t = ms_cl_iadd(c, t, ms_cl_load_ext(c, 32, 1, MS_CL_I64, 0, ta, 8));                  /* -1 */
  t = ms_cl_iadd(c, t, ms_cl_load(c, MS_CL_I64, MS_CL_TRUSTED, ta, 0));                 /* 10 */
  ms_cl_atomic_rmw(c, MS_CL_RMW_ADD, MS_CL_I64, 0, ms_cl_data_addr(c, d_cnt), ms_cl_i64const(c, 5));
  ms_cl_block two = ms_cl_block_new(c), other = ms_cl_block_new(c);
  ms_cl_switch(c, x, other, (uint64_t[]){2}, (ms_cl_block[]){two}, 1);
  ms_cl_enter(c, two);
  ms_cl_ret1(c, ms_cl_call1(c, f_add, (ms_cl_val[]){t, ms_cl_i64const(c, 1000)}, 2));
  ms_cl_enter(c, other);
  ms_cl_ret1(c, t);
  ms_cl_seal_all(c);
  check(ms_cl_func_end(c), "ms_mem");

  ms_cl_func_begin(c, f_ind);
  b = ms_cl_entry(c);
  ms_cl_val fp = ms_cl_load(c, MS_CL_I64, 0, ms_cl_data_addr(c, d_ptr), 0);
  ms_cl_ret1(c, ms_cl_call_indirect1(c, s_ii, fp, (ms_cl_val[]){ms_cl_block_param(c, b, 0), ms_cl_block_param(c, b, 1)}, 2));
  ms_cl_seal_all(c);
  check(ms_cl_func_end(c), "ms_indirect");

  uint32_t nloc = 0;
  const ms_cl_srcloc_range *locs = ms_cl_func_srclocs(c, f_chk, &nloc);
  size_t len = 0;
  const uint8_t *o = ms_cl_finish(c, &len);
  check(!o, "finish");
  FILE *fh = fopen(argv[2], "wb");
  if (!fh || fwrite(o, 1, len, fh) != len) { fprintf(stderr, "cannot write %s\n", argv[2]); return 1; }
  fclose(fh);
  int found = 0;
  for (uint32_t j = 0; j < nloc; j++) if (locs[j].loc == 7) found = 1;
  printf("object %zu bytes, %u source ranges%s\n", len, nloc, found ? ", loc 7 present" : ", loc 7 MISSING");
  ms_cl_free(c);
  return found ? 0 : 1;
}
