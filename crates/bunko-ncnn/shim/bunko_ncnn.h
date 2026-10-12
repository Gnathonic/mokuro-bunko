/* hayai-nova and paddle-manga on ncnn Vulkan: the C ABI the Rust side calls
 * (docs/rust-port/VULKAN.md).
 *
 * The host preprocesses each crop (resize, patchify, position rows / taps, RoPE tables:
 * bunko-vlm) and hands over per-crop arrays; everything after that runs here: the
 * vision graph per crop, one batched decoder prefill, then the decode loop with the KV
 * cache kept on the GPU between steps and the argmax in the graph (only token ids come
 * back). Every call returns BN_OK or an error code with a message in `err`; nothing
 * aborts the process. One handle runs one batch at a time (internally locked); handles
 * on the same device share pipelines and weights.
 */
#ifndef BUNKO_NCNN_H
#define BUNKO_NCNN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define BN_OK 0
#define BN_ERR_ARG 1
#define BN_ERR_LOAD 2
#define BN_ERR_RUN 3
#define BN_ERR_VRAM 4 /* the model does not fit the device's free memory */

/* flags for the loads and bn_self_check */
#define BN_FLAG_FP16_STORAGE 2 /* fp16 blobs and weights, fp32 math: faster, NOT 0.5.2 text parity */

/* bn_self_check bits: the custom layer passed its check (else the stock path is used) */
#define BN_CHECK_GEMM 1
#define BN_CHECK_ATTN 2
#define BN_CHECK_ARGMAX 4

typedef struct bn_hayai bn_hayai;
typedef struct bn_paddle bn_paddle;

typedef struct bn_gpu_info_t {
    char name[256];
    int32_t type; /* 0 discrete, 1 integrated, 2 virtual, 3 cpu (software: never used) */
    uint32_t vendor_id;
    uint32_t device_id;
    uint32_t api_version;
    uint32_t driver_version;
    uint32_t subgroup_size;
    int32_t fp16_storage;
    uint32_t heap_budget_mib; /* free device-local memory now */
} bn_gpu_info_t;

/* Vulkan devices ncnn sees. */
int bn_gpu_count(void);
/* Destroy ncnn's Vulkan instance now (idempotent). Hosts call it from an exit handler
 * that runs before the Vulkan driver is unloaded: ncnn's own static destructor runs at
 * library unload, after the loader has freed the driver on Windows (0xC0000005). */
void bn_shutdown(void);
int bn_gpu_name(int index, char* buf, size_t len);
int bn_gpu_type(int index);
int bn_gpu_info(int index, bn_gpu_info_t* out);

/* Runs the custom layers' checks on a device (cached per process); `passed` gets the
 * BN_CHECK_* bits, `log` what failed. */
int bn_self_check(int gpu, uint32_t flags, uint32_t* passed, char* log, size_t loglen);

/* hayai-nova. dir: vision.param/.bin, prefill.param + step.param sharing decoder.bin,
 * token_embeddings.f32 (16004 x 512 fp32). */
bn_hayai* bn_hayai_load(const char* dir, int gpu, uint32_t flags, char* err, size_t errlen);
void bn_hayai_free(bn_hayai* h);
/* One batch of n crops. Crop i has hp[i] x wp[i] patches; pv[i] and pos[i] hold
 * hp*wp rows of 768 floats. Writes up to max_new ids per crop to
 * out_ids[i * max_new ...] (EOS/PAD removed) and their count to out_len[i]. */
int bn_hayai_generate(bn_hayai* h, int n, const int32_t* hp, const int32_t* wp, const float* const* pv,
                      const float* const* pos, int32_t max_new, int32_t* out_ids, int32_t* out_len,
                      char* err, size_t errlen);
/* Seconds in vision, prefill, (unused), decode, and the decode steps run. */
void bn_hayai_stats(const bn_hayai* h, double out_secs[4], int64_t* out_steps);

/* paddle-manga. dir: vision.param/.bin, prefill.param + step.param sharing decoder.bin,
 * embeddings.f32 (vocab x 1024), pos_table.f32 (729 x 1152). The chat prompt around the
 * image tokens comes from the caller (ncnn.json). */
bn_paddle* bn_paddle_load(const char* dir, int gpu, uint32_t flags, const int32_t* prefix, int32_t n_prefix,
                          const int32_t* suffix, int32_t n_suffix, int32_t vocab, char* err, size_t errlen);
void bn_paddle_free(bn_paddle* h);
/* One batch of n crops (left-padded together). Crop i: gh[i] x gw[i] patches (even),
 * pixels[i] gh*gw rows of 588 (3x14x14), pos_idx[i]/pos_w[i] gh*gw x 4 taps into the
 * position table, vcos[i]/vsin[i] gh*gw x 36 RoPE halves; caps[i] its token cap.
 * Writes up to max_cap ids per crop to out_ids[i * max_cap ...], counts to out_len. */
int bn_paddle_generate(bn_paddle* h, int n, const int32_t* gh, const int32_t* gw, const float* const* pixels,
                       const int32_t* const* pos_idx, const float* const* pos_w, const float* const* vcos,
                       const float* const* vsin, const int32_t* caps, int32_t max_cap, int32_t* out_ids,
                       int32_t* out_len, char* err, size_t errlen);
void bn_paddle_stats(const bn_paddle* h, double out_secs[4], int64_t* out_steps);

#ifdef __cplusplus
}
#endif

#endif
