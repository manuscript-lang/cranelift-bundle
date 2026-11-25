/* Loads the shared library by path, compiles f(a, b) = a + b and checks for an object file with that symbol. */
#include "../include/ms_cranelift.h"
#include <stdio.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
#define LOAD(p) ((void *)LoadLibraryA(p))
#define SYM(h, n) ((void *)GetProcAddress((HMODULE)(h), n))
#else
#include <dlfcn.h>
#define LOAD(p) dlopen(p, RTLD_NOW | RTLD_LOCAL)
#define SYM(h, n) dlsym(h, n)
#endif
#define FN(h, name) __typeof__(&name) p_##name = (__typeof__(&name))SYM(h, #name); if (!p_##name) { fprintf(stderr, "missing %s\n", #name); return 1; }
int main(int argc, char **argv) {
  if (argc < 2) return 2;
  void *h = LOAD(argv[1]);
  if (!h) { fprintf(stderr, "cannot load %s\n", argv[1]); return 1; }
  FN(h, ms_cl_new) FN(h, ms_cl_declare) FN(h, ms_cl_begin) FN(h, ms_cl_block) FN(h, ms_cl_enter)
  FN(h, ms_cl_param) FN(h, ms_cl_bin) FN(h, ms_cl_ret) FN(h, ms_cl_seal_all) FN(h, ms_cl_end) FN(h, ms_cl_finish)
  ms_cl *c = p_ms_cl_new(0);
  uint32_t f = p_ms_cl_declare(c, "ms_smoke_add", 2, 0);
  p_ms_cl_begin(c, f, 2);
  uint32_t b = p_ms_cl_block(c);
  p_ms_cl_enter(c, b, 1);
  uint32_t x = p_ms_cl_param(c, b, 0), y = p_ms_cl_param(c, b, 1);
  p_ms_cl_ret(c, p_ms_cl_bin(c, 0, x, y));
  p_ms_cl_seal_all(c);
  p_ms_cl_end(c);
  size_t n = 0;
  const uint8_t *o = p_ms_cl_finish(c, &n);
  int found = 0;
  for (size_t i = 0; n > 12 && i + 12 <= n; i++) if (!memcmp(o + i, "ms_smoke_add", 12)) { found = 1; break; }
  printf("object %zu bytes, symbol %s\n", n, found ? "found" : "MISSING");
  return n > 64 && found ? 0 : 1;
}
