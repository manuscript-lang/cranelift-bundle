/* C API of the Manuscript Cranelift shim (spike subset: i64 values only). */
#ifndef MS_CRANELIFT_H
#define MS_CRANELIFT_H
#include <stddef.h>
#include <stdint.h>
typedef struct ms_cl ms_cl;
ms_cl *ms_cl_new(int32_t speed);
uint32_t ms_cl_declare(ms_cl *, const char *name, uint32_t nparams, int32_t import);
void ms_cl_begin(ms_cl *, uint32_t func, uint32_t nparams);
uint32_t ms_cl_block(ms_cl *);
uint32_t ms_cl_block_param(ms_cl *, uint32_t blk);
void ms_cl_enter(ms_cl *, uint32_t blk, int32_t entry);
uint32_t ms_cl_param(ms_cl *, uint32_t blk, uint32_t i);
uint32_t ms_cl_iconst(ms_cl *, int64_t k);
uint32_t ms_cl_bin(ms_cl *, uint32_t op, uint32_t x, uint32_t y);
uint32_t ms_cl_chk(ms_cl *, uint32_t op, uint32_t x, uint32_t y, uint32_t *flag);
uint32_t ms_cl_icmp(ms_cl *, uint32_t cc, uint32_t x, uint32_t y);
void ms_cl_jump(ms_cl *, uint32_t blk, const uint32_t *args, uint32_t n);
void ms_cl_brif(ms_cl *, uint32_t cond, uint32_t t, const uint32_t *ta, uint32_t tn, uint32_t e, const uint32_t *ea, uint32_t en);
uint32_t ms_cl_call(ms_cl *, uint32_t func, const uint32_t *args, uint32_t n);
void ms_cl_ret(ms_cl *, uint32_t v);
void ms_cl_seal_all(ms_cl *);
void ms_cl_cold(ms_cl *, uint32_t blk);
void ms_cl_end(ms_cl *);
const uint8_t *ms_cl_finish(ms_cl *, size_t *len);
#endif
