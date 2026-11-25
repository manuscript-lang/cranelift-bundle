/* Runs the functions smoke.c generated. */
#include <stdint.h>
#include <stdio.h>
extern int64_t ms_add(int64_t, int64_t), ms_sum(int64_t), ms_checked(int64_t, int64_t), ms_float(double),
    ms_mem(int64_t), ms_indirect(int64_t, int64_t), pf_0(int64_t), pf_5(int64_t), pf_17(int64_t), pf_63(int64_t);
extern int64_t ms_counter;
extern const uintptr_t ms_pcs[3];
static int bad;
#define EXPECT(e, want) do { int64_t g = (e); if (g != (want)) { printf("FAIL %s = %lld, want %lld\n", #e, (long long)g, (long long)(want)); bad = 1; } } while (0)
int main(void) {
  EXPECT(ms_add(40, 2), 42);
  EXPECT(ms_sum(100), 5050);
  EXPECT(ms_checked(1, 2), 3);
  EXPECT(ms_checked(INT64_MAX, 1), -1);
  EXPECT(ms_float(3.0), 3);          /* sqrt(10) = 3.16 */
  EXPECT(ms_float(1.0e9), 1000);     /* clamped by the select */
  EXPECT(ms_mem(5), 255 + 5 - 1 + 10);
  EXPECT(ms_mem(0x18002), (255 + (int64_t)(int16_t)0x8002 - 1 + 10) + 0); /* x != 2: the low 16 bits are 0x8002 */
  EXPECT(ms_mem(2), 255 + 2 - 1 + 10 + 1000);
  EXPECT(ms_counter, 15);
  EXPECT(ms_indirect(20, 22), 42);
  EXPECT(pf_0(7), 7);
  EXPECT(pf_5(10), 15);
  EXPECT(pf_17(100), 117);
  EXPECT(pf_63(1), 64);
  EXPECT(ms_pcs[0], (int64_t)((uintptr_t)&ms_add + 4));   /* a function address plus an offset */
  EXPECT(ms_pcs[1], (int64_t)(uintptr_t)&ms_sum);
  EXPECT(ms_pcs[2], (int64_t)((uintptr_t)&ms_pcs + 8));    /* the table points into itself */
  puts(bad ? "FAILED" : "all ok");
  return bad;
}
