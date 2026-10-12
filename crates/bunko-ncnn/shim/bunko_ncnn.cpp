// hayai-nova and paddle-manga on ncnn Vulkan: see bunko_ncnn.h.
//
// Layout: the custom Vulkan layers (BnGemm, BnAttn, BnArgmax) and their per-device
// registry (shared pipelines, deduplicated weights, the load-time self-check that
// falls back to ncnn's stock layers), then the two engines' decode loops.
//
// Shaders make no subgroup-size assumption: 16x16 (GEMM / attention) or 256-wide
// (softmax / argmax) workgroups and shared-memory reductions only, so wave32 (RDNA)
// and wave64 (GCN: Polaris, Vega) run the same code.
#include "bunko_ncnn.h"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <map>
#include <memory>
#include <mutex>
#include <random>
#include <string>
#include <tuple>
#include <vector>

#include "command.h"
#include "cpu.h"
#include "datareader.h"
#include "gpu.h"
#include "layer.h"
#include "modelbin.h"
#include "net.h"
#include "pipeline.h"

namespace {

constexpr float NEG_HAYAI = -1e9f;
constexpr float NEG_PADDLE = -3.4028234663852886e38f;  // torch.finfo(float32).min

void set_err(char* err, size_t len, const std::string& s) {
    if (err && len) {
        size_t n = std::min(len - 1, s.size());
        std::memcpy(err, s.data(), n);
        err[n] = 0;
    }
}

double now() {
    using namespace std::chrono;
    return duration<double>(steady_clock::now().time_since_epoch()).count();
}

bool env_on(const char* name) {
    const char* v = std::getenv(name);
    return v && v[0] == '1';
}

// A row-major (rows, cols) fp32 host Mat that owns its data (zeros unless src).
ncnn::Mat mat2(int cols, int rows, const float* src = nullptr) {
    ncnn::Mat m(cols, rows, (size_t)4u);
    if (src) std::memcpy(m.data, src, (size_t)cols * rows * 4);
    else m.fill(0.f);
    return m;
}

ncnn::Mat unpack(const ncnn::Mat& m) {
    if (m.elempack == 1) return m;
    ncnn::Mat out;
    ncnn::convert_packing(m, out, 1);
    return out;
}

int argmax_row(const float* p, int n) {
    int best = 0;
    float bv = p[0];
    for (int i = 1; i < n; i++)
        if (p[i] > bv) { bv = p[i]; best = i; }  // first index on ties, like torch.argmax
    return best;
}

class DataReaderFromEmpty : public ncnn::DataReader {
public:
    int scan(const char*, void*) const override { return 0; }
    size_t read(void* buf, size_t size) const override {
        std::memset(buf, 0, size);
        return size;
    }
};

// ---------------------------------------------------------------------------------------
// shaders (ncnn's storage macros: sfp = fp32, or fp16 under use_fp16_storage; math fp32)

// out(M,N) = A(M,K) * B(N,K)^T + bias(N). TM x TN tile, 16x16 threads, RM x RN per thread.
const char* SGEMM_BODY = R"GLSL(
#define TK 16
#define RM (TM / 16)
#define RN (TN / 16)
layout(local_size_x = 16, local_size_y = 16, local_size_z = 1) in;
layout(binding = 0) readonly buffer A_blob { sfpvec4 a4[]; };
layout(binding = 1) readonly buffer B_blob { sfpvec4 b4[]; };
layout(binding = 2) readonly buffer C_blob { sfp bias[]; };
#if SPLITK
layout(binding = 3) writeonly buffer O_blob { float o[]; };  // fp32 partials, [split][M][N]
#else
layout(binding = 3) writeonly buffer O_blob { sfp o[]; };
#endif
layout(push_constant) uniform parameter { int M; int N; int K; int ks; } p;
shared float As[TK][TM + 1];
shared float Bs[TK][TN + 1];
void main()
{
    const int tx = int(gl_LocalInvocationID.x);
    const int ty = int(gl_LocalInvocationID.y);
    const int tid = ty * 16 + tx;
    const int row0 = int(gl_WorkGroupID.y) * TM;
    const int col0 = int(gl_WorkGroupID.x) * TN;
    float acc[RM][RN];
    for (int i = 0; i < RM; i++) for (int j = 0; j < RN; j++) acc[i][j] = 0.f;
    const int K4 = p.K / 4;
#if SPLITK
    const int kbeg = int(gl_WorkGroupID.z) * p.ks, kend = min(p.K, kbeg + p.ks);
#else
    const int kbeg = 0, kend = p.K;
#endif
    for (int k0 = kbeg; k0 < kend; k0 += TK)
    {
        for (int e = tid; e < TM * (TK / 4); e += 256)
        {
            int r = e / (TK / 4), kq = e % (TK / 4);
            int gr = row0 + r, gk = k0 / 4 + kq;
            vec4 v = (gr < p.M && gk < K4) ? vec4(buffer_ld4(a4, gr * K4 + gk)) : vec4(0.f);
            As[kq * 4 + 0][r] = v.x; As[kq * 4 + 1][r] = v.y; As[kq * 4 + 2][r] = v.z; As[kq * 4 + 3][r] = v.w;
        }
        for (int e = tid; e < TN * (TK / 4); e += 256)
        {
            int r = e / (TK / 4), kq = e % (TK / 4);
            int gr = col0 + r, gk = k0 / 4 + kq;
            vec4 v = (gr < p.N && gk < K4) ? vec4(buffer_ld4(b4, gr * K4 + gk)) : vec4(0.f);
            Bs[kq * 4 + 0][r] = v.x; Bs[kq * 4 + 1][r] = v.y; Bs[kq * 4 + 2][r] = v.z; Bs[kq * 4 + 3][r] = v.w;
        }
        barrier();
        for (int kk = 0; kk < TK; kk++)
        {
            float av[RM], bv[RN];
            for (int i = 0; i < RM; i++) av[i] = As[kk][ty + 16 * i];
            for (int j = 0; j < RN; j++) bv[j] = Bs[kk][tx + 16 * j];
            for (int i = 0; i < RM; i++) for (int j = 0; j < RN; j++) acc[i][j] = fma(av[i], bv[j], acc[i][j]);
        }
        barrier();
    }
    for (int i = 0; i < RM; i++)
    {
        int r = row0 + ty + 16 * i;
        if (r >= p.M) continue;
        for (int j = 0; j < RN; j++)
        {
            int c = col0 + tx + 16 * j;
#if SPLITK
            if (c < p.N) o[(int(gl_WorkGroupID.z) * p.M + r) * p.N + c] = acc[i][j];
#else
            if (c < p.N) buffer_st1(o, r * p.N + c, afp(acc[i][j] + float(buffer_ld1(bias, c))));
#endif
        }
    }
}
)GLSL";

// Split-K reduce: out(M,N) = bias + sum over the splits of the fp32 partials, in order.
const char* SPLITK_REDUCE_BODY = R"GLSL(
layout(local_size_x = 256, local_size_y = 1, local_size_z = 1) in;
layout(binding = 0) readonly buffer P_blob { float part[]; };
layout(binding = 1) readonly buffer C_blob { sfp bias[]; };
layout(binding = 2) writeonly buffer O_blob { sfp o[]; };
layout(push_constant) uniform parameter { int M; int N; int splits; } p;
void main()
{
    const int i = int(gl_GlobalInvocationID.x);
    if (i >= p.M * p.N) return;
    float s = 0.f;
    for (int z = 0; z < p.splits; z++) s += part[z * p.M * p.N + i];
    buffer_st1(o, i, afp(s + float(buffer_ld1(bias, i % p.N))));
}
)GLSL";

// Batched GEMM for attention, per channel z (64x64 tile, scalar loads):
//   TRANSB=1: O[z][o_row0 + r] = alpha * A[z][a_row0 + r] . B[z / bdiv]^T (+ mask)
//   TRANSB=0: O[z][o_row0 + r] = A[z][a_row0 + r] . B[z / bdiv]
// mask (HAS_MASK): channel z / mdiv, row m_row0 + r (row 0 when mrows == 1), added.
const char* BMM_BODY = R"GLSL(
#define TM 64
#define TN 64
#define TK 16
layout(local_size_x = 16, local_size_y = 16, local_size_z = 1) in;
layout(binding = 0) readonly buffer A_blob { sfp a[]; };
layout(binding = 1) readonly buffer B_blob { sfp b[]; };
layout(binding = 2) writeonly buffer O_blob { sfp o[]; };
layout(binding = 3) readonly buffer M_blob { sfp mk[]; };
layout(push_constant) uniform parameter {
    int M; int N; int K; int astep; int bstep; int ostep; float alpha;
    int a_row0; int o_row0; int bdiv; int mstep; int mdiv; int mrows; int m_row0;
} p;
shared float As[TK][TM + 1];
shared float Bs[TK][TN + 1];
void main()
{
    const int tx = int(gl_LocalInvocationID.x);
    const int ty = int(gl_LocalInvocationID.y);
    const int tid = ty * 16 + tx;
    const int row0 = int(gl_WorkGroupID.y) * TM;
    const int col0 = int(gl_WorkGroupID.x) * TN;
    const int z = int(gl_WorkGroupID.z);
    const int ao = z * p.astep + p.a_row0 * p.K, bo = (z / p.bdiv) * p.bstep, oo = z * p.ostep + p.o_row0 * p.N;
    float acc[4][4];
    for (int i = 0; i < 4; i++) for (int j = 0; j < 4; j++) acc[i][j] = 0.f;
    for (int k0 = 0; k0 < p.K; k0 += TK)
    {
        for (int e = tid; e < TM * TK; e += 256)
        {
            int r = e / TK, kk = e % TK;
            int gr = row0 + r, gk = k0 + kk;
            As[kk][r] = (gr < p.M && gk < p.K) ? float(buffer_ld1(a, ao + gr * p.K + gk)) : 0.f;
        }
#if TRANSB
        for (int e = tid; e < TN * TK; e += 256)
        {
            int r = e / TK, kk = e % TK;
            int gr = col0 + r, gk = k0 + kk;
            Bs[kk][r] = (gr < p.N && gk < p.K) ? float(buffer_ld1(b, bo + gr * p.K + gk)) : 0.f;
        }
#else
        for (int e = tid; e < TN * TK; e += 256)
        {
            int kk = e / TN, c = e % TN;
            int gk = k0 + kk, gc = col0 + c;
            Bs[kk][c] = (gk < p.K && gc < p.N) ? float(buffer_ld1(b, bo + gk * p.N + gc)) : 0.f;
        }
#endif
        barrier();
        for (int kk = 0; kk < TK; kk++)
        {
            float av[4], bv[4];
            for (int i = 0; i < 4; i++) av[i] = As[kk][ty + 16 * i];
            for (int j = 0; j < 4; j++) bv[j] = Bs[kk][tx + 16 * j];
            for (int i = 0; i < 4; i++) for (int j = 0; j < 4; j++) acc[i][j] = fma(av[i], bv[j], acc[i][j]);
        }
        barrier();
    }
    for (int i = 0; i < 4; i++)
    {
        int r = row0 + ty + 16 * i;
        if (r >= p.M) continue;
        for (int j = 0; j < 4; j++)
        {
            int c = col0 + tx + 16 * j;
            if (c >= p.N) continue;
            float v = acc[i][j] * p.alpha;
#if HAS_MASK
            v += float(buffer_ld1(mk, (z / p.mdiv) * p.mstep + (p.mrows == 1 ? 0 : p.m_row0 + r) * p.N + c));
#endif
            buffer_st1(o, oo + r * p.N + c, afp(v));
        }
    }
}
)GLSL";

// In-place softmax over each row (w) of a (c, h, w) blob, one 256-thread group per row.
const char* SOFTMAX_BODY = R"GLSL(
layout(local_size_x = 256, local_size_y = 1, local_size_z = 1) in;
layout(binding = 0) buffer X_blob { sfp x[]; };
layout(push_constant) uniform parameter { int w; int h; int cstep; } p;
shared float red[256];
void main()
{
    const int row = int(gl_WorkGroupID.x);
    const int z = row / p.h, y = row % p.h;
    const int base = z * p.cstep + y * p.w;
    const int t = int(gl_LocalInvocationID.x);
    float m = -3.402823466e+38;
    for (int i = t; i < p.w; i += 256) m = max(m, float(buffer_ld1(x, base + i)));
    red[t] = m;
    barrier();
    for (int s = 128; s > 0; s >>= 1) { if (t < s) red[t] = max(red[t], red[t + s]); barrier(); }
    m = red[0];
    barrier();
    float sum = 0.f;
    for (int i = t; i < p.w; i += 256) sum += exp(float(buffer_ld1(x, base + i)) - m);
    red[t] = sum;
    barrier();
    for (int s = 128; s > 0; s >>= 1) { if (t < s) red[t] += red[t + s]; barrier(); }
    sum = red[0];
    for (int i = t; i < p.w; i += 256) buffer_st1(x, base + i, afp(exp(float(buffer_ld1(x, base + i)) - m) / sum));
}
)GLSL";

// Row argmax of (h, w) -> (h) as floats (exact below 2^24), first index on ties.
const char* ARGMAX_BODY = R"GLSL(
layout(local_size_x = 256, local_size_y = 1, local_size_z = 1) in;
layout(binding = 0) readonly buffer X_blob { sfp x[]; };
layout(binding = 1) writeonly buffer O_blob { float o[]; };
layout(push_constant) uniform parameter { int w; } p;
shared float bv[256];
shared int bi[256];
void main()
{
    const int row = int(gl_WorkGroupID.x);
    const int t = int(gl_LocalInvocationID.x);
    float best = -3.402823466e+38;
    int idx = 0x7fffffff;
    for (int i = t; i < p.w; i += 256)
    {
        float v = float(buffer_ld1(x, row * p.w + i));
        if (v > best) { best = v; idx = i; }
    }
    bv[t] = best; bi[t] = idx;
    barrier();
    for (int s = 128; s > 0; s >>= 1)
    {
        if (t < s)
        {
            float ov = bv[t + s]; int oi = bi[t + s];
            if (ov > bv[t] || (ov == bv[t] && oi < bi[t])) { bv[t] = ov; bi[t] = oi; }
        }
        barrier();
    }
    if (t == 0) o[row] = float(bi[0]);
}
)GLSL";

// ---------------------------------------------------------------------------------------
// per-device registry: pipelines, shared weights, self-check results (under g_mu)

using WeightKey = std::tuple<int, int, int, bool>;  // id, N, K, fp16 storage

struct DevState {
    int handles = 0;
    std::map<std::string, ncnn::Pipeline*> pipelines;
    ncnn::VkAllocator* weights = nullptr;  // owns the shared weights' memory
    std::map<WeightKey, std::pair<ncnn::VkMat, ncnn::VkMat>> gemm_weights;
    std::map<WeightKey, int> gemm_refs;
    // per storage mode (fp32 / fp16): -1 untested, 0 failed, 1 passed
    int gemm_ok[2] = {-1, -1}, attn_ok[2] = {-1, -1}, argmax_ok[2] = {-1, -1};
    std::string check_log;
};

std::mutex g_mu;
std::map<const ncnn::VulkanDevice*, DevState> g_dev;

int mode_of(const ncnn::Option& opt) { return opt.use_fp16_storage ? 1 : 0; }

const ncnn::Pipeline* get_pipeline(const ncnn::VulkanDevice* dev, const ncnn::Option& opt, const std::string& key,
                                   const std::string& defines, const char* body, int lx, int ly) {
    std::lock_guard<std::mutex> g(g_mu);
    DevState& st = g_dev[dev];
    const std::string k = key + (opt.use_fp16_storage ? "/fp16" : "/fp32");
    auto it = st.pipelines.find(k);
    if (it != st.pipelines.end()) return it->second;
    const std::string src = "#version 450\n" + defines + body;
    std::vector<uint32_t> spirv;
    ncnn::Option o = opt;
    o.use_fp16_arithmetic = false;
    o.use_fp16_packed = false;
    if (ncnn::compile_spirv_module(src.c_str(), o, spirv) != 0) return nullptr;
    auto* p = new ncnn::Pipeline(dev);
    p->set_local_size_xyz(lx, ly, 1);
    if (p->create(spirv.data(), spirv.size() * 4, std::vector<ncnn::vk_specialization_type>()) != 0) {
        delete p;
        return nullptr;
    }
    st.pipelines[k] = p;
    return p;
}

void retain_device(const ncnn::VulkanDevice* dev) {
    std::lock_guard<std::mutex> g(g_mu);
    DevState& st = g_dev[dev];
    if (st.handles++ == 0 && !st.weights) st.weights = new ncnn::VkWeightAllocator(dev);
}

// The last handle on a device frees its pipelines and the shared weights' allocator
// (every net is cleared first, so no layer still holds a buffer from it).
void release_device(const ncnn::VulkanDevice* dev) {
    std::lock_guard<std::mutex> g(g_mu);
    auto it = g_dev.find(dev);
    if (it == g_dev.end() || --it->second.handles > 0) return;
    DevState& st = it->second;
    for (auto& p : st.pipelines) delete p.second;
    st.pipelines.clear();
    st.gemm_weights.clear();
    st.gemm_refs.clear();
    delete st.weights;
    st.weights = nullptr;
}

// call with g_mu held
int& ok_flag(const ncnn::VulkanDevice* dev, int which, const ncnn::Option& opt) {
    DevState& st = g_dev[dev];
    const int m = mode_of(opt);
    return which == 0 ? st.gemm_ok[m] : which == 1 ? st.attn_ok[m] : st.argmax_ok[m];
}

bool layer_ok(const ncnn::VulkanDevice* dev, int which, const ncnn::Option& opt) {
    std::lock_guard<std::mutex> g(g_mu);
    return ok_flag(dev, which, opt) != 0;  // untested = while the check itself runs
}

// ---------------------------------------------------------------------------------------
// BnGemm: y(M,N) = x(M,K) * W(N,K)^T + b. params 0=N 1=K 2=bias 3=weight id.
// Layers with the same weight id on a device share one GPU copy (prefill and step).

const int GEMM_TILES[4][2] = {{128, 64}, {64, 64}, {64, 32}, {16, 64}};

int gemm_variant(int M, int N) {
    if (M <= 16) return 3;
    if ((M + 127) / 128 * ((N + 63) / 64) >= 128) return 0;
    if ((M + 63) / 64 * ((N + 63) / 64) >= 128) return 1;
    return 2;
}

class BnGemm : public ncnn::Layer {
public:
    BnGemm() {
        one_blob_only = true;
        support_vulkan = true;
        support_packing = false;
        support_vulkan_packing = false;
        support_fp16_storage = true;
        support_bf16_storage = false;
    }
    ~BnGemm() override { delete stock; }
    int load_param(const ncnn::ParamDict& pd) override {
        N = pd.get(0, 0);
        K = pd.get(1, 0);
        bias_term = pd.get(2, 0);
        wid = pd.get(3, 0);
        return 0;
    }
    int load_model(const ncnn::ModelBin& mb) override {
        W = mb.load(K, N, 1);
        if (W.empty()) return -100;
        if (bias_term) {
            B = mb.load(N, 1);
        } else {
            B.create(N);
            B.fill(0.f);
        }
        return B.empty() ? -100 : 0;
    }
#if NCNN_VULKAN
    int create_pipeline(const ncnn::Option& opt) override {
        if (!vkdev) return 0;
        if (K % 4 != 0 || !layer_ok(vkdev, 0, opt)) return create_stock(opt);
        for (int v = 0; v < 4; v++) {
            const std::string d = "#define TM " + std::to_string(GEMM_TILES[v][0]) + "\n#define TN " +
                                  std::to_string(GEMM_TILES[v][1]) + "\n#define SPLITK 0\n";
            pipelines[v] = get_pipeline(vkdev, opt, "sgemm" + std::to_string(v), d, SGEMM_BODY, 16, 16);
            if (!pipelines[v]) return create_stock(opt);
        }
        split = get_pipeline(vkdev, opt, "sgemm_split", "#define TM 16\n#define TN 64\n#define SPLITK 1\n", SGEMM_BODY, 16, 16);
        reduce = get_pipeline(vkdev, opt, "sgemm_reduce", "", SPLITK_REDUCE_BODY, 256, 1);
        if (!split || !reduce) return create_stock(opt);
        return 0;
    }
    // ncnn's own Gemm on the same weights (a device where BnGemm failed its check)
    int create_stock(const ncnn::Option& opt) {
        stock = ncnn::create_layer_vulkan("Gemm");
        if (!stock) return -1;
        stock->vkdev = vkdev;
        ncnn::ParamDict pd;
        pd.set(3, 1);   // transB
        pd.set(5, 1);   // constant B
        pd.set(6, 1);   // constant C
        pd.set(8, N);
        pd.set(9, K);
        pd.set(10, 4);  // C per output column
        stock->load_param(pd);
        ncnn::Mat arr[2] = {W, B.reshape(N, 1)};
        ncnn::ModelBinFromMatArray mb(arr);
        if (stock->load_model(mb) != 0) return -1;
        return stock->create_pipeline(opt);
    }
    int destroy_pipeline(const ncnn::Option& opt) override {
        if (stock) stock->destroy_pipeline(opt);
        if (shared_key_valid) {
            std::lock_guard<std::mutex> g(g_mu);
            DevState& st = g_dev[vkdev];
            if (--st.gemm_refs[shared_key] <= 0) {
                st.gemm_weights.erase(shared_key);
                st.gemm_refs.erase(shared_key);
            }
            shared_key_valid = false;
        }
        W_gpu.release();
        B_gpu.release();
        return 0;
    }
    int upload_model(ncnn::VkTransfer& cmd, const ncnn::Option& opt) override {
        if (stock) return stock->upload_model(cmd, opt);
        shared_key = std::make_tuple(wid, N, K, opt.use_fp16_storage);
        ncnn::VkAllocator* alloc = nullptr;
        {
            std::lock_guard<std::mutex> g(g_mu);
            DevState& st = g_dev[vkdev];
            auto it = st.gemm_weights.find(shared_key);
            if (it != st.gemm_weights.end()) {
                W_gpu = it->second.first;
                B_gpu = it->second.second;
                st.gemm_refs[shared_key]++;
                shared_key_valid = true;
            }
            alloc = st.weights;
        }
        if (!shared_key_valid) {
            ncnn::Option o = opt;
            o.use_fp16_packed = false;
            o.blob_vkallocator = alloc;  // outlives this net: another net may share the buffer
            cmd.record_upload(W, W_gpu, o);
            cmd.record_upload(B, B_gpu, o);
            if (W_gpu.empty() || B_gpu.empty()) return -100;
            std::lock_guard<std::mutex> g(g_mu);
            DevState& st = g_dev[vkdev];
            st.gemm_weights[shared_key] = {W_gpu, B_gpu};
            st.gemm_refs[shared_key] = 1;
            shared_key_valid = true;
        }
        if (opt.lightmode) {
            W.release();
            B.release();
        }
        return 0;
    }
    int forward(const ncnn::VkMat& x, ncnn::VkMat& y, ncnn::VkCompute& cmd, const ncnn::Option& opt) const override {
        if (stock) return stock->forward(x, y, cmd, opt);
        const int splits = split_count(x.h, N, K);
        if (splits > 1) return run_split(x, y, cmd, opt, splits);
        return run(x, y, cmd, opt, gemm_variant(x.h, N));
    }
    // A decode step's few rows over a long K leave most of the GPU idle (N/64 groups):
    // split K into up to 8 slices of >= 256, then sum the slices in order.
    static int split_count(int M, int N, int K) {
        if (M > 16 || env_on("BN_NO_SPLITK")) return 1;
        const int groups = (N + 63) / 64;
        if (groups >= 64 || K < 512) return 1;
        return std::max(1, std::min({8, K / 256, (96 + groups - 1) / groups}));
    }
    int run_split(const ncnn::VkMat& x, ncnn::VkMat& y, ncnn::VkCompute& cmd, const ncnn::Option& opt, int splits) const {
        const size_t es = opt.use_fp16_storage ? 2u : 4u;
        if (x.dims != 2 || x.w != K || x.elempack != 1 || x.elemsize != es) return run(x, y, cmd, opt, gemm_variant(x.h, N));
        const int M = x.h;
        const int ks = ((K + splits - 1) / splits + 15) / 16 * 16;
        ncnn::VkMat part;
        part.create(N * M, splits, (size_t)4u, 1, opt.workspace_vkallocator);
        y.create(N, M, es, 1, opt.blob_vkallocator);
        if (part.empty() || y.empty()) return -100;
        std::vector<ncnn::vk_constant_type> c(4);
        c[0].i = M;
        c[1].i = N;
        c[2].i = K;
        c[3].i = ks;
        ncnn::VkMat d1;
        d1.w = (N + 63) / 64 * 16;
        d1.h = (M + 15) / 16 * 16;
        d1.c = splits;
        cmd.record_pipeline(split, {x, W_gpu, B_gpu, part}, c, d1);
        std::vector<ncnn::vk_constant_type> c2(3);
        c2[0].i = M;
        c2[1].i = N;
        c2[2].i = splits;
        ncnn::VkMat d2;
        d2.w = M * N;
        d2.h = 1;
        d2.c = 1;
        cmd.record_pipeline(reduce, {part, B_gpu, y}, c2, d2);
        return 0;
    }
    int run(const ncnn::VkMat& x, ncnn::VkMat& y, ncnn::VkCompute& cmd, const ncnn::Option& opt, int v) const {
        const size_t es = opt.use_fp16_storage ? 2u : 4u;
        if (x.dims != 2 || x.w != K || x.elempack != 1 || x.elemsize != es) {
            NCNN_LOGE("BnGemm: unexpected input dims=%d w=%d h=%d elempack=%d elemsize=%zu (K=%d)", x.dims, x.w, x.h,
                      x.elempack, x.elemsize, K);
            return -1;
        }
        const int M = x.h;
        y.create(N, M, es, 1, opt.blob_vkallocator);
        if (y.empty()) return -100;
        std::vector<ncnn::vk_constant_type> c(4);
        c[0].i = M;
        c[1].i = N;
        c[2].i = K;
        c[3].i = K;
        const int tm = GEMM_TILES[v][0], tn = GEMM_TILES[v][1];
        ncnn::VkMat disp;
        disp.w = (N + tn - 1) / tn * 16;
        disp.h = (M + tm - 1) / tm * 16;
        disp.c = 1;
        cmd.record_pipeline(pipelines[v], {x, W_gpu, B_gpu, y}, c, disp);
        return 0;
    }
#endif
    int forward(const ncnn::Mat& x, ncnn::Mat& y, const ncnn::Option& opt) const override {
        const int M = x.h;
        y.create(N, M, 4u, opt.blob_allocator);
        if (y.empty()) return -100;
        for (int i = 0; i < M; i++) {
            const float* a = x.row(i);
            float* o = y.row(i);
            for (int j = 0; j < N; j++) {
                const float* w = (const float*)W.data + (size_t)j * K;
                double sum = ((const float*)B.data)[j];
                for (int k = 0; k < K; k++) sum += (double)a[k] * w[k];
                o[j] = (float)sum;
            }
        }
        return 0;
    }

    int N = 0, K = 0, bias_term = 0, wid = 0;
    ncnn::Mat W, B;
    ncnn::Layer* stock = nullptr;
#if NCNN_VULKAN
    ncnn::VkMat W_gpu, B_gpu;
    const ncnn::Pipeline* pipelines[4] = {nullptr, nullptr, nullptr, nullptr};
    const ncnn::Pipeline *split = nullptr, *reduce = nullptr;
    WeightKey shared_key;
    bool shared_key_valid = false;
#endif
};

// ---------------------------------------------------------------------------------------
// BnAttn: out = softmax(scale * q k^T [+ mask]) v per q channel, query rows in chunks
// (BN_ATTN_MB of scores at a time, default 128 MiB: paddle's 5120-patch crops need 1.7 GB
// for the whole matrix). inputs q (D, Mq, Cq), k (D, Lk, Ck), v (Dv, Lk, Ck), [mask (Lk,
// Mq|1, Cq/mdiv)]; kv channel = z / (Cq/Ck). params 0=scale 1=has_mask 2=mask_div.

class BnAttn : public ncnn::Layer {
public:
    BnAttn() {
        one_blob_only = false;
        support_vulkan = true;
        support_packing = false;
        support_vulkan_packing = false;
        support_fp16_storage = true;
    }
    ~BnAttn() override { delete stock; }
    int load_param(const ncnn::ParamDict& pd) override {
        scale = pd.get(0, 1.f);
        has_mask = pd.get(1, 0);
        mdiv = pd.get(2, 1);
        return 0;
    }
#if NCNN_VULKAN
    int create_pipeline(const ncnn::Option& opt) override {
        if (!vkdev) return 0;
        if (!layer_ok(vkdev, 1, opt)) {
            if (has_mask) return 0;  // forward reports it: masked attention has no stock path
            stock = ncnn::create_layer_vulkan("SDPA");
            if (!stock) return -1;
            stock->vkdev = vkdev;
            ncnn::ParamDict pd;
            pd.set(5, 0);
            pd.set(6, scale);
            stock->load_param(pd);
            return stock->create_pipeline(opt);
        }
        qk = get_pipeline(vkdev, opt, has_mask ? "bmm_qk_mask" : "bmm_qk",
                          has_mask ? "#define TRANSB 1\n#define HAS_MASK 1\n" : "#define TRANSB 1\n#define HAS_MASK 0\n",
                          BMM_BODY, 16, 16);
        sv = get_pipeline(vkdev, opt, "bmm_sv", "#define TRANSB 0\n#define HAS_MASK 0\n", BMM_BODY, 16, 16);
        sm = get_pipeline(vkdev, opt, "softmax", "", SOFTMAX_BODY, 256, 1);
        return qk && sv && sm ? 0 : -1;
    }
    int destroy_pipeline(const ncnn::Option& opt) override {
        if (stock) stock->destroy_pipeline(opt);
        return 0;
    }
    int forward(const std::vector<ncnn::VkMat>& in, std::vector<ncnn::VkMat>& out, ncnn::VkCompute& cmd,
                const ncnn::Option& opt) const override {
        if (stock) return stock->forward(in, out, cmd, opt);
        if (!qk) {
            NCNN_LOGE("BnAttn: failed its self-check on this device and masked attention has no stock path");
            return -1;
        }
        return run(in, out, cmd, opt, 0);
    }
    int run(const std::vector<ncnn::VkMat>& in, std::vector<ncnn::VkMat>& out, ncnn::VkCompute& cmd,
            const ncnn::Option& opt, int chunk_rows) const {
        const ncnn::VkMat& q = in[0];
        const ncnn::VkMat& k = in[1];
        const ncnn::VkMat& v = in[2];
        const size_t es = opt.use_fp16_storage ? 2u : 4u;
        if (q.dims != 3 || k.dims != 3 || v.dims != 3 || q.elempack != 1 || k.elempack != 1 || v.elempack != 1 ||
            q.elemsize != es || q.w != k.w || k.h != v.h || k.c != v.c || k.c == 0 || q.c % k.c != 0) {
            NCNN_LOGE("BnAttn: bad shapes q(%d,%d,%d) k(%d,%d,%d) v(%d,%d,%d)", q.w, q.h, q.c, k.w, k.h, k.c, v.w, v.h, v.c);
            return -1;
        }
        const int Cq = q.c, Mq = q.h, D = q.w, Lk = k.h, Dv = v.w;
        const int rep = Cq / k.c;
        ncnn::VkMat mask;
        if (has_mask) {
            mask = in[3];
            if (mask.elempack != 1 || mask.w != Lk || (mask.h != Mq && mask.h != 1) || mask.c * mdiv < Cq) {
                NCNN_LOGE("BnAttn: bad mask (%d,%d,%d) for q (%d,%d,%d), k rows %d", mask.w, mask.h, mask.c, q.w, q.h,
                          q.c, Lk);
                return -1;
            }
        }
        ncnn::VkMat& o = out[0];
        o.create(Dv, Mq, Cq, es, 1, opt.blob_vkallocator);
        if (o.empty()) return -100;
        int rows = chunk_rows;
        if (rows <= 0) {
            const char* mb = std::getenv("BN_ATTN_MB");
            const double budget = (mb ? std::atof(mb) : 128.0) * 1024 * 1024;
            rows = (int)std::max(16.0, std::floor(budget / ((double)Cq * Lk * es)));
        }
        rows = std::max(1, std::min({rows, Mq, 65535 / Cq}));  // softmax groups = Cq * rows
        ncnn::VkMat s;
        s.create(Lk, rows, Cq, es, 1, opt.workspace_vkallocator);
        if (s.empty()) return -100;
        for (int r0 = 0; r0 < Mq; r0 += rows) {
            const int m = std::min(rows, Mq - r0);
            std::vector<ncnn::vk_constant_type> c(14);
            c[0].i = m; c[1].i = Lk; c[2].i = D;
            c[3].i = (int)q.cstep; c[4].i = (int)k.cstep; c[5].i = (int)s.cstep;
            c[6].f = scale; c[7].i = r0; c[8].i = 0; c[9].i = rep;
            c[10].i = has_mask ? (int)mask.cstep : 0; c[11].i = mdiv; c[12].i = has_mask ? mask.h : 1; c[13].i = r0;
            ncnn::VkMat d1;
            d1.w = (Lk + 63) / 64 * 16;
            d1.h = (m + 63) / 64 * 16;
            d1.c = Cq;
            cmd.record_pipeline(qk, {q, k, s, has_mask ? mask : s}, c, d1);
            std::vector<ncnn::vk_constant_type> cs(3);
            cs[0].i = Lk; cs[1].i = m; cs[2].i = (int)s.cstep;
            ncnn::VkMat ds;
            ds.w = Cq * m * 256;
            ds.h = 1;
            ds.c = 1;
            cmd.record_pipeline(sm, {s}, cs, ds);
            std::vector<ncnn::vk_constant_type> c2(14);
            c2[0].i = m; c2[1].i = Dv; c2[2].i = Lk;
            c2[3].i = (int)s.cstep; c2[4].i = (int)v.cstep; c2[5].i = (int)o.cstep;
            c2[6].f = 1.f; c2[7].i = 0; c2[8].i = r0; c2[9].i = rep;
            c2[10].i = 0; c2[11].i = 1; c2[12].i = 1; c2[13].i = 0;
            ncnn::VkMat d2;
            d2.w = (Dv + 63) / 64 * 16;
            d2.h = (m + 63) / 64 * 16;
            d2.c = Cq;
            cmd.record_pipeline(sv, {s, v, o, s}, c2, d2);
        }
        return 0;
    }
#endif
    int forward(const std::vector<ncnn::Mat>& in, std::vector<ncnn::Mat>& out, const ncnn::Option& opt) const override {
        const ncnn::Mat &q = in[0], &k = in[1], &v = in[2];
        const int Cq = q.c, Mq = q.h, D = q.w, Lk = k.h, Dv = v.w, rep = Cq / k.c;
        ncnn::Mat& o = out[0];
        o.create(Dv, Mq, Cq, 4u, opt.blob_allocator);
        if (o.empty()) return -100;
        std::vector<double> sc(Lk);
        for (int z = 0; z < Cq; z++)
            for (int r = 0; r < Mq; r++) {
                const float* qr = q.channel(z).row(r);
                double mx = -1e300;
                for (int j = 0; j < Lk; j++) {
                    const float* kr = k.channel(z / rep).row(j);
                    double d = 0;
                    for (int t = 0; t < D; t++) d += (double)qr[t] * kr[t];
                    d *= scale;
                    if (has_mask) d += in[3].channel(z / mdiv).row(in[3].h == 1 ? 0 : r)[j];
                    sc[j] = d;
                    mx = std::max(mx, d);
                }
                double sum = 0;
                for (int j = 0; j < Lk; j++) sum += std::exp(sc[j] - mx);
                float* orow = o.channel(z).row(r);
                for (int t = 0; t < Dv; t++) {
                    double acc = 0;
                    for (int j = 0; j < Lk; j++) acc += std::exp(sc[j] - mx) / sum * v.channel(z / rep).row(j)[t];
                    orow[t] = (float)acc;
                }
            }
        return 0;
    }

    float scale = 1.f;
    int has_mask = 0, mdiv = 1;
    ncnn::Layer* stock = nullptr;
#if NCNN_VULKAN
    const ncnn::Pipeline *qk = nullptr, *sv = nullptr, *sm = nullptr;
#endif
};

// ---------------------------------------------------------------------------------------
// BnArgmax: (V, b) -> (b) row argmax as floats. Where it failed its self-check the
// engines read "logits" and take the argmax on the host instead.

class BnArgmax : public ncnn::Layer {
public:
    BnArgmax() {
        one_blob_only = true;
        support_vulkan = true;
        support_packing = false;
        support_vulkan_packing = false;
        support_fp16_storage = true;
    }
#if NCNN_VULKAN
    int create_pipeline(const ncnn::Option& opt) override {
        if (!vkdev) return 0;
        pipe = get_pipeline(vkdev, opt, "argmax", "", ARGMAX_BODY, 256, 1);
        return pipe ? 0 : -1;
    }
    int forward(const ncnn::VkMat& x, ncnn::VkMat& y, ncnn::VkCompute& cmd, const ncnn::Option& opt) const override {
        if (x.elempack != 1 || x.dims > 2) return -1;
        const int rows = x.dims == 2 ? x.h : 1;
        y.create(rows, (size_t)4u, 1, opt.blob_vkallocator);
        if (y.empty()) return -100;
        std::vector<ncnn::vk_constant_type> c(1);
        c[0].i = x.w;
        ncnn::VkMat d;
        d.w = rows * 256;
        d.h = 1;
        d.c = 1;
        cmd.record_pipeline(pipe, {x, y}, c, d);
        return 0;
    }
    const ncnn::Pipeline* pipe = nullptr;
#endif
    int forward(const ncnn::Mat& x, ncnn::Mat& y, const ncnn::Option& opt) const override {
        const int rows = x.dims == 2 ? x.h : 1;
        y.create(rows, (size_t)4u, opt.blob_allocator);
        if (y.empty()) return -100;
        for (int r = 0; r < rows; r++) ((float*)y.data)[r] = (float)argmax_row(x.row(r), x.w);
        return 0;
    }
};

ncnn::Layer* bn_gemm_creator(void*) { return new BnGemm; }
ncnn::Layer* bn_attn_creator(void*) { return new BnAttn; }
ncnn::Layer* bn_argmax_creator(void*) { return new BnArgmax; }

void register_layers(ncnn::Net& net) {
    net.register_custom_layer("BnGemm", bn_gemm_creator);
    net.register_custom_layer("BnAttn", bn_attn_creator);
    net.register_custom_layer("BnArgmax", bn_argmax_creator);
}

// ---------------------------------------------------------------------------------------
// self-check: each custom layer on small odd shapes against its CPU path (double sums)

#if NCNN_VULKAN
struct GpuScratch {
    ncnn::VulkanDevice* dev;
    ncnn::Option opt;
    GpuScratch(ncnn::VulkanDevice* d, const ncnn::Option& base) : dev(d), opt(base) {
        opt.blob_vkallocator = dev->acquire_blob_allocator();
        opt.workspace_vkallocator = opt.blob_vkallocator;
        opt.staging_vkallocator = dev->acquire_staging_allocator();
    }
    ~GpuScratch() {
        dev->reclaim_blob_allocator(opt.blob_vkallocator);
        dev->reclaim_staging_allocator(opt.staging_vkallocator);
    }
    ncnn::VkMat up(const ncnn::Mat& m) {
        ncnn::Mat src = m;
        if (opt.use_fp16_storage) ncnn::cast_float32_to_float16(m, src, opt);
        ncnn::VkMat v;
        ncnn::VkCompute cmd(dev);
        cmd.record_clone(src, v, opt);
        cmd.submit_and_wait();
        return v;
    }
    ncnn::Mat down(const ncnn::VkMat& v) {
        ncnn::Mat m;
        ncnn::VkCompute cmd(dev);
        cmd.record_clone(v, m, opt);
        cmd.submit_and_wait();
        if (m.elembits() == 16) {
            ncnn::Mat f;
            ncnn::cast_float16_to_float32(m, f, opt);
            m = f;
        }
        return unpack(m);
    }
};

ncnn::Mat rnd(int w, int h, int c, std::mt19937& g) {
    ncnn::Mat m = c > 0 ? ncnn::Mat(w, h, c, (size_t)4u) : ncnn::Mat(w, h, (size_t)4u);
    std::uniform_real_distribution<float> u(-1.f, 1.f);
    for (int z = 0; z < std::max(c, 1); z++) {
        float* p = c > 0 ? (float*)m.channel(z) : (float*)m.data;
        for (int i = 0; i < w * h; i++) p[i] = u(g);
    }
    return m;
}

double maxdiff(const ncnn::Mat& a, const ncnn::Mat& b) {
    if (a.w != b.w || a.h != b.h || a.c != b.c || a.empty()) return 1e30;
    double d = 0;
    for (int z = 0; z < a.c; z++) {
        const float* pa = a.dims == 3 ? (const float*)a.channel(z) : (const float*)a.data;
        const float* pb = b.dims == 3 ? (const float*)b.channel(z) : (const float*)b.data;
        for (int i = 0; i < a.w * a.h; i++) d = std::max(d, (double)std::fabs(pa[i] - pb[i]));
    }
    return d;
}

// Once per (device, storage mode); the results live in the registry.
void self_check(ncnn::VulkanDevice* dev, const ncnn::Option& base) {
    const int md = mode_of(base);
    {
        std::lock_guard<std::mutex> g(g_mu);
        if (g_dev[dev].gemm_ok[md] != -1) return;
    }
    if (env_on("BN_NO_CUSTOM")) {  // test switch: behave as if every check had failed
        std::lock_guard<std::mutex> g(g_mu);
        DevState& st = g_dev[dev];
        st.gemm_ok[md] = st.attn_ok[md] = st.argmax_ok[md] = 0;
        st.check_log += "custom layers disabled (BN_NO_CUSTOM)\n";
        return;
    }
    const double tol = base.use_fp16_storage ? 3e-2 : 1e-4;
    std::mt19937 rng(7);
    GpuScratch gs(dev, base);
    ncnn::Option cpu;
    cpu.use_vulkan_compute = false;
    cpu.num_threads = 1;
    std::string log;
    int gemm = 1, attn = 1, amax = 1;
    {  // BnGemm: every tile on M = 5, 37 and 129 rows (K 96, N 72, bias)
        BnGemm L;
        L.vkdev = dev;
        ncnn::ParamDict pd;
        pd.set(0, 72);
        pd.set(1, 96);
        pd.set(2, 1);
        pd.set(3, 0x5e1fc4ec);
        L.load_param(pd);
        ncnn::Mat arr[2] = {rnd(96, 72, 0, rng), rnd(72, 1, 0, rng).reshape(72)};
        ncnn::ModelBinFromMatArray mb(arr);
        ncnn::Option o = gs.opt;
        o.lightmode = false;
        if (L.load_model(mb) != 0 || L.create_pipeline(o) != 0 || L.stock) {
            gemm = 0;
            log += "BnGemm: pipeline creation failed\n";
        } else {
            ncnn::VkTransfer t(dev);
            L.upload_model(t, o);
            t.submit_and_wait();
            for (int M : {5, 37, 129})
                for (int v = 0; v < 4 && gemm; v++) {
                    ncnn::Mat x = rnd(96, M, 0, rng), ref;
                    L.forward(x, ref, cpu);
                    ncnn::VkMat vx = gs.up(x), vy;
                    ncnn::VkCompute cmd(dev);
                    if (L.run(vx, vy, cmd, o, v) != 0 || cmd.submit_and_wait() != 0) {
                        gemm = 0;
                        log += "BnGemm: run failed\n";
                        break;
                    }
                    const double d = maxdiff(gs.down(vy), ref);
                    if (!(d <= tol * 10)) {
                        gemm = 0;
                        log += "BnGemm: tile " + std::to_string(v) + " M=" + std::to_string(M) + " max diff " + std::to_string(d) + "\n";
                    }
                }
            if (gemm) {  // the split-K path: few rows over a long K (K 1024)
                BnGemm S;
                S.vkdev = dev;
                ncnn::ParamDict ps;
                ps.set(0, 72);
                ps.set(1, 1024);
                ps.set(2, 1);
                ps.set(3, 0x5e1fc4ed);
                S.load_param(ps);
                ncnn::Mat sa[2] = {rnd(1024, 72, 0, rng), rnd(72, 1, 0, rng).reshape(72)};
                ncnn::ModelBinFromMatArray smb(sa);
                if (S.load_model(smb) != 0 || S.create_pipeline(o) != 0 || S.stock) {
                    gemm = 0;
                } else {
                    ncnn::VkTransfer t2(dev);
                    S.upload_model(t2, o);
                    t2.submit_and_wait();
                    ncnn::Mat x = rnd(1024, 5, 0, rng), ref;
                    S.forward(x, ref, cpu);
                    ncnn::VkMat vy;
                    ncnn::VkCompute cmd(dev);
                    if (S.run_split(gs.up(x), vy, cmd, o, 4) != 0 || cmd.submit_and_wait() != 0 || !(maxdiff(gs.down(vy), ref) <= tol * 30)) {
                        gemm = 0;
                        log += "BnGemm: split-K mismatch\n";
                    }
                    S.destroy_pipeline(o);
                }
            }
            L.destroy_pipeline(o);
        }
    }
    {  // BnAttn: GQA rep 2, per-row mask (mdiv 2), forced 7-row chunks, odd sizes
        BnAttn L;
        L.vkdev = dev;
        ncnn::ParamDict pd;
        pd.set(0, 0.125f);
        pd.set(1, 1);
        pd.set(2, 2);
        L.load_param(pd);
        if (L.create_pipeline(gs.opt) != 0 || !L.qk) {
            attn = 0;
            log += "BnAttn: pipeline creation failed\n";
        } else {
            ncnn::Mat q = rnd(72, 37, 4, rng), k = rnd(72, 29, 2, rng), v = rnd(40, 29, 2, rng), m = rnd(29, 37, 2, rng);
            std::vector<ncnn::Mat> ref(1);
            L.forward({q, k, v, m}, ref, cpu);
            std::vector<ncnn::VkMat> vin{gs.up(q), gs.up(k), gs.up(v), gs.up(m)}, vout(1);
            ncnn::VkCompute cmd(dev);
            if (L.run(vin, vout, cmd, gs.opt, 7) != 0 || cmd.submit_and_wait() != 0) {
                attn = 0;
                log += "BnAttn: run failed\n";
            } else {
                const double d = maxdiff(gs.down(vout[0]), ref[0]);
                if (!(d <= tol)) {
                    attn = 0;
                    log += "BnAttn: max diff " + std::to_string(d) + "\n";
                }
            }
        }
    }
    {  // BnArgmax: 3 rows of 1000 with a planted tie (the first index wins)
        BnArgmax L;
        L.vkdev = dev;
        if (L.create_pipeline(gs.opt) != 0) {
            amax = 0;
        } else {
            ncnn::Mat x = rnd(1000, 3, 0, rng);
            x.row(1)[10] = 5.f;
            x.row(1)[900] = 5.f;
            ncnn::Mat ref;
            L.forward(x, ref, cpu);
            ncnn::VkMat vy;
            ncnn::VkCompute cmd(dev);
            if (L.forward(gs.up(x), vy, cmd, gs.opt) != 0 || cmd.submit_and_wait() != 0) amax = 0;
            else if (maxdiff(gs.down(vy), ref) != 0) amax = 0;
        }
        if (!amax) log += "BnArgmax: mismatch\n";
    }
    std::lock_guard<std::mutex> g(g_mu);
    DevState& st = g_dev[dev];
    st.gemm_ok[md] = gemm;
    st.attn_ok[md] = attn;
    st.argmax_ok[md] = amax;
    st.check_log += log;
    if (!log.empty()) NCNN_LOGE("bunko-ncnn self-check on %s: %s", dev->info.device_name(), log.c_str());
}
#endif

// ---------------------------------------------------------------------------------------
// an engine on one Vulkan device

struct Engine {
    std::mutex mu;  // one batch at a time per handle
    ncnn::Option opt, dl;
    ncnn::VulkanDevice* vkdev = nullptr;
    ncnn::VkAllocator* blob_alloc = nullptr;
    ncnn::VkAllocator* ws_alloc = nullptr;
    ncnn::VkAllocator* staging_alloc = nullptr;
    std::vector<std::unique_ptr<ncnn::Net>> nets;
    std::vector<std::unique_ptr<ncnn::Net>> xcat;  // by input count
    bool gpu_argmax = true;
    double secs[4] = {0, 0, 0, 0};
    int64_t steps = 0;
    std::string dir;

    ~Engine() {
        if (env_on("BN_STATS"))
            std::fprintf(stderr, "bunko-ncnn %s: vision %.2f s, prefill %.2f s, decode %.2f s, %lld steps\n", dir.c_str(),
                         secs[0], secs[1], secs[3], (long long)steps);
        xcat.clear();
        nets.clear();  // layers drop their shared weights first
        if (vkdev) {
            if (blob_alloc) vkdev->reclaim_blob_allocator(blob_alloc);
            if (ws_alloc) vkdev->reclaim_blob_allocator(ws_alloc);
            if (staging_alloc) vkdev->reclaim_staging_allocator(staging_alloc);
            release_device(vkdev);
        }
    }

    int init(int gpu, uint32_t flags, std::string& why) {
#if NCNN_VULKAN
        if (gpu < 0 || gpu >= ncnn::get_gpu_count()) {
            why = "no Vulkan device " + std::to_string(gpu);
            return BN_ERR_ARG;
        }
        ncnn::Option& o = opt;
        o.lightmode = true;
        o.num_threads = 1;
        o.use_vulkan_compute = true;
        o.use_fp16_packed = o.use_fp16_storage = (flags & BN_FLAG_FP16_STORAGE) != 0;
        o.use_fp16_arithmetic = false;
        o.use_bf16_storage = o.use_bf16_packed = false;
        o.use_int8_storage = o.use_int8_arithmetic = false;
        vkdev = ncnn::get_gpu_device(gpu);
        if (!vkdev) {
            why = "cannot open Vulkan device " + std::to_string(gpu);
            return BN_ERR_LOAD;
        }
        retain_device(vkdev);
        blob_alloc = vkdev->acquire_blob_allocator();
        ws_alloc = vkdev->acquire_blob_allocator();
        staging_alloc = vkdev->acquire_staging_allocator();
        dl = o;
        dl.blob_vkallocator = blob_alloc;
        dl.workspace_vkallocator = ws_alloc;
        dl.staging_vkallocator = staging_alloc;
        self_check(vkdev, o);
        std::lock_guard<std::mutex> g(g_mu);
        gpu_argmax = ok_flag(vkdev, 2, o) != 0;
        return BN_OK;
#else
        (void)gpu;
        (void)flags;
        why = "built without Vulkan";
        return BN_ERR_LOAD;
#endif
    }

    // VRAM: the weights files plus a working margin must fit the device's free budget.
    int check_vram(const std::vector<std::string>& bins, double margin_mb, std::string& why) {
        double need = margin_mb;
        for (const auto& b : bins) {
            FILE* f = std::fopen((dir + "/" + b).c_str(), "rb");
            if (!f) continue;
            std::fseek(f, 0, SEEK_END);
            need += (double)std::ftell(f) / 1048576.0 * (opt.use_fp16_storage ? 0.5 : 1.0);
            std::fclose(f);
        }
        uint32_t budget = vkdev->get_heap_budget();
        if (const char* fake = std::getenv("BN_FAKE_BUDGET_MIB")) budget = (uint32_t)std::atoi(fake);  // tests
        if (budget > 0 && need > budget && !env_on("BN_IGNORE_VRAM")) {
            char buf[256];
            std::snprintf(buf, sizeof buf, "needs about %.0f MiB of GPU memory, %s has %u MiB free", need,
                          vkdev->info.device_name(), budget);
            why = buf;
            return BN_ERR_VRAM;
        }
        return BN_OK;
    }

    ncnn::Net* load_net(const std::string& param, const std::string& bin, std::string& why) {
        auto net = std::make_unique<ncnn::Net>();
        net->opt = opt;
        net->set_vulkan_device(vkdev);
        register_layers(*net);
        if (net->load_param((dir + "/" + param).c_str()) != 0 || net->load_model((dir + "/" + bin).c_str()) != 0) {
            why = "cannot load " + dir + "/" + param + " with " + bin;
            return nullptr;
        }
        nets.push_back(std::move(net));
        return nets.back().get();
    }

    ncnn::Extractor ex(ncnn::Net& net) {
        ncnn::Extractor e = net.create_extractor();
        e.set_blob_vkallocator(blob_alloc);
        e.set_workspace_vkallocator(ws_alloc);
        e.set_staging_vkallocator(staging_alloc);
        return e;
    }

    // One Concat along rows of n inputs in0..in{n-1} -> x.
    ncnn::Net* xcat_net(int n) {
        if ((int)xcat.size() <= n) xcat.resize(n + 1);
        if (!xcat[n]) {
            std::string p = "7767517\n" + std::to_string(n + 1) + " " + std::to_string(n + 1) + "\n";
            for (int i = 0; i < n; i++) p += "Input in" + std::to_string(i) + " 0 1 in" + std::to_string(i) + "\n";
            p += "Concat cat " + std::to_string(n) + " 1";
            for (int i = 0; i < n; i++) p += " in" + std::to_string(i);
            p += " x 0=0\n";
            auto net = std::make_unique<ncnn::Net>();
            net->opt = opt;
            net->set_vulkan_device(vkdev);
            if (net->load_param_mem(p.c_str()) != 0) return nullptr;
            net->load_model(DataReaderFromEmpty());
            xcat[n] = std::move(net);
        }
        return xcat[n].get();
    }

    // The next ids of a graph run: the in-graph argmax, or the logits for the host.
    int record_ids(ncnn::Extractor& e, ncnn::VkCompute& cmd, ncnn::Mat& host, ncnn::VkMat& keep) {
        if (e.extract(gpu_argmax ? "ids" : "logits", keep, cmd) != 0) return -1;
        cmd.record_download(keep, host, dl);
        return 0;
    }
    std::vector<int> ids_from(const ncnn::Mat& raw, int b) {
        ncnn::Mat m = unpack(raw);
        if (m.elembits() == 16) {
            ncnn::Mat f;
            ncnn::cast_float16_to_float32(m, f, dl);
            m = f;
        }
        std::vector<int> out(b);
        for (int i = 0; i < b; i++)
            out[i] = gpu_argmax ? (int)((const float*)m.data)[i] : argmax_row(m.row(i), m.w);
        return out;
    }
};

std::vector<float> read_f32(const std::string& path, size_t count, std::string& why) {
    std::vector<float> v;
    FILE* f = std::fopen(path.c_str(), "rb");
    if (!f) {
        why = "cannot open " + path;
        return v;
    }
    v.resize(count);
    const size_t got = std::fread(v.data(), 4, count, f);
    std::fclose(f);
    if (got != count) {
        why = path + " is short";
        v.clear();
    }
    return v;
}

#define BN_TRY_BEGIN try {
#define BN_TRY_END                                                        \
    }                                                                     \
    catch (const std::exception& e) {                                     \
        set_err(err, errlen, std::string("internal error: ") + e.what()); \
        return BN_ERR_RUN;                                                \
    }                                                                     \
    catch (...) {                                                         \
        set_err(err, errlen, "internal error");                           \
        return BN_ERR_RUN;                                                \
    }

// hayai-nova
constexpr int H_DV = 768, H_DM = 512, H_VOCAB = 16004, H_LAYERS = 12, H_Q = 8, H_AXIS = 32;
constexpr int H_BOS = 16001, H_EOS = 16002, H_PAD = 16000;

// paddle-manga
constexpr int P_DV = 1152, P_DT = 1024, P_LAYERS = 18, P_HD = 128, P_VHALF = 36, P_SIDE = 27, P_PATCH_DIM = 588;
constexpr int P_EOS = 2, P_PAD = 0;
constexpr int P_MROPE[3] = {16, 24, 24};

}  // namespace

// C++ only (profiling tools): the shim's custom layers on a bare ncnn::Net.
void bn_register_layers(ncnn::Net& net) { register_layers(net); }

struct bn_hayai : Engine {
    ncnn::Net *vision = nullptr, *prefill = nullptr, *step = nullptr;
    std::vector<float> emb;  // H_VOCAB x H_DM
};

struct bn_paddle : Engine {
    ncnn::Net *vision = nullptr, *prefill = nullptr, *step = nullptr;
    std::vector<float> emb;        // vocab x P_DT
    std::vector<float> pos_table;  // 729 x P_DV
    int vocab = 0;
    std::vector<int> prefix, suffix;
    float text_inv[P_HD / 2] = {};
};

namespace {
// M-RoPE angles of position (t, h, w): the cos/sin halves (64 each), as 0.5.2's sections.
void mrope(const bn_paddle* h, long t, long hh, long w, float* c, float* s) {
    const float pos[3] = {(float)t, (float)hh, (float)w};
    int o = 0;
    for (int axis = 0; axis < 3; axis++) {
        for (int k = o; k < o + P_MROPE[axis]; k++) {
            const float a = pos[axis] * h->text_inv[k];
            c[k] = std::cos(a);
            s[k] = std::sin(a);
        }
        o += P_MROPE[axis];
    }
}

template <class E>
E* load_engine(const char* dir, int gpu, uint32_t flags, double margin_mb, char* err, size_t errlen,
               std::unique_ptr<E>& h) {
    std::string why;
    h->dir = dir;
    if (h->init(gpu, flags, why) != BN_OK || h->check_vram({"vision.bin", "decoder.bin"}, margin_mb, why) != BN_OK) {
        set_err(err, errlen, why);
        return nullptr;
    }
    h->vision = h->load_net("vision.param", "vision.bin", why);
    if (h->vision) h->prefill = h->load_net("prefill.param", "decoder.bin", why);
    if (h->prefill) h->step = h->load_net("step.param", "decoder.bin", why);
    if (!h->step) {
        set_err(err, errlen, why);
        return nullptr;
    }
    return h.get();
}
}  // namespace

extern "C" {

void bn_shutdown(void) {
#if NCNN_VULKAN
    ncnn::destroy_gpu_instance();
#endif
}

int bn_gpu_count(void) {
#if NCNN_VULKAN
    return ncnn::get_gpu_count();
#else
    return 0;
#endif
}

int bn_gpu_name(int index, char* buf, size_t len) {
#if NCNN_VULKAN
    if (index < 0 || index >= ncnn::get_gpu_count()) return BN_ERR_ARG;
    set_err(buf, len, ncnn::get_gpu_info(index).device_name());
    return BN_OK;
#else
    (void)index;
    set_err(buf, len, "");
    return BN_ERR_ARG;
#endif
}

int bn_gpu_type(int index) {
#if NCNN_VULKAN
    if (index < 0 || index >= ncnn::get_gpu_count()) return -1;
    return ncnn::get_gpu_info(index).type();
#else
    (void)index;
    return -1;
#endif
}

int bn_gpu_info(int index, bn_gpu_info_t* out) {
#if NCNN_VULKAN
    if (!out || index < 0 || index >= ncnn::get_gpu_count()) return BN_ERR_ARG;
    const ncnn::GpuInfo& g = ncnn::get_gpu_info(index);
    std::memset(out, 0, sizeof *out);
    set_err(out->name, sizeof out->name, g.device_name());
    out->type = g.type();
    out->vendor_id = g.vendor_id();
    out->device_id = g.device_id();
    out->api_version = g.api_version();
    out->driver_version = g.driver_version();
    out->subgroup_size = g.subgroup_size();
    out->fp16_storage = g.support_fp16_storage();
    const ncnn::VulkanDevice* d = ncnn::get_gpu_device(index);
    out->heap_budget_mib = d ? d->get_heap_budget() : 0;
    return BN_OK;
#else
    (void)index;
    (void)out;
    return BN_ERR_ARG;
#endif
}

int bn_self_check(int gpu, uint32_t flags, uint32_t* passed, char* log, size_t loglen) {
#if NCNN_VULKAN
    if (gpu < 0 || gpu >= ncnn::get_gpu_count()) return BN_ERR_ARG;
    ncnn::VulkanDevice* dev = ncnn::get_gpu_device(gpu);
    if (!dev) return BN_ERR_LOAD;
    ncnn::Option o;
    o.use_vulkan_compute = true;
    o.use_fp16_packed = o.use_fp16_storage = (flags & BN_FLAG_FP16_STORAGE) != 0;
    o.use_fp16_arithmetic = false;
    o.use_bf16_storage = o.use_bf16_packed = false;
    retain_device(dev);
    try {
        self_check(dev, o);
    } catch (...) {
    }
    {
        std::lock_guard<std::mutex> g(g_mu);
        const DevState& st = g_dev[dev];
        const int m = mode_of(o);
        if (passed)
            *passed = (st.gemm_ok[m] == 1 ? BN_CHECK_GEMM : 0) | (st.attn_ok[m] == 1 ? BN_CHECK_ATTN : 0) |
                      (st.argmax_ok[m] == 1 ? BN_CHECK_ARGMAX : 0);
        set_err(log, loglen, st.check_log);
    }
    release_device(dev);
    return BN_OK;
#else
    (void)gpu;
    (void)flags;
    (void)passed;
    set_err(log, loglen, "built without Vulkan");
    return BN_ERR_LOAD;
#endif
}

// ---- hayai ------------------------------------------------------------------------------

bn_hayai* bn_hayai_load(const char* dir, int gpu, uint32_t flags, char* err, size_t errlen) {
    try {
        auto h = std::make_unique<bn_hayai>();
        if (!load_engine(dir, gpu, flags, 768.0, err, errlen, h)) return nullptr;
        std::string why;
        h->emb = read_f32(h->dir + "/token_embeddings.f32", (size_t)H_VOCAB * H_DM, why);
        if (h->emb.empty()) {
            set_err(err, errlen, why);
            return nullptr;
        }
        return h.release();
    } catch (const std::exception& e) {
        set_err(err, errlen, std::string("internal error: ") + e.what());
        return nullptr;
    }
}

void bn_hayai_free(bn_hayai* h) { delete h; }

void bn_hayai_stats(const bn_hayai* h, double out_secs[4], int64_t* out_steps) {
    for (int i = 0; i < 4; i++) out_secs[i] = h->secs[i];
    *out_steps = h->steps;
}

int bn_hayai_generate(bn_hayai* h, int n, const int32_t* hp, const int32_t* wp, const float* const* pv,
                      const float* const* pos, int32_t max_new, int32_t* out_ids, int32_t* out_len, char* err,
                      size_t errlen) {
    if (!h || n <= 0 || max_new <= 0) return BN_ERR_ARG;
    std::lock_guard<std::mutex> lock(h->mu);
    BN_TRY_BEGIN
    const int b = n;
    std::vector<float> f(H_AXIS / 2);
    for (int i = 0; i < H_AXIS / 2; i++) f[i] = 1.0f / std::pow(10000.0f, (float)(2 * i) / H_AXIS);
    std::vector<int> valid(b), wo(b);
    int m = 0;
    for (int i = 0; i < b; i++) {
        if (hp[i] <= 0 || wp[i] <= 0) return BN_ERR_ARG;
        wo[i] = (wp[i] + 1) / 2;
        valid[i] = ((hp[i] + 1) / 2) * wo[i];
        m = std::max(m, valid[i]);
    }
    const int l0 = m + 1;
    // vision per crop (its own patch count: no padding, no mask)
    const double tv = now();
    std::vector<ncnn::VkMat> vis(b);
    std::vector<ncnn::Mat> tails(b);
    for (int i = 0; i < b; i++) {
        const int P = hp[i] * wp[i], M = valid[i];
        ncnn::Mat sel = mat2(P, 4 * M);
        for (int t = 0; t < M; t++) {
            const int y = t / wo[i], x = t % wo[i];
            const int d[4][2] = {{0, 0}, {0, 1}, {1, 0}, {1, 1}};
            for (int k = 0; k < 4; k++) {
                const int idx = std::min(2 * y + d[k][0], hp[i] - 1) * wp[i] + std::min(2 * x + d[k][1], wp[i] - 1);
                ((float*)sel.row(t * 4 + k))[idx] = 1.f;
            }
        }
        ncnn::Mat mpv = mat2(H_DV, P, pv[i]), mpos = mat2(H_DV, P, pos[i]);
        ncnn::VkCompute cmd(h->vkdev);
        ncnn::Extractor e = h->ex(*h->vision);
        e.input("pv", mpv);
        e.input("pos", mpos);
        e.input("sel", sel);
        if (e.extract("vis", vis[i], cmd) != 0 || cmd.submit_and_wait() != 0) {
            set_err(err, errlen, "vision failed");
            return BN_ERR_RUN;
        }
        tails[i] = mat2(H_DM, l0 - M);  // zero padding to the longest crop, then BOS
        std::memcpy(tails[i].row(l0 - M - 1), &h->emb[(size_t)H_BOS * H_DM], H_DM * 4);
    }
    h->secs[0] += now() - tv;
    // one prefill for the batch: per head mask (L0, L0, 8b), RoPE rows (32, b*L0)
    const double tp = now();
    ncnn::Mat mask(l0, l0, H_Q * b, (size_t)4u), cs = mat2(H_AXIS, b * l0), sn = mat2(H_AXIS, b * l0);
    mask.fill(0.f);
    for (int i = 0; i < b; i++) {
        const int M = valid[i];
        for (int hh = 0; hh < H_Q; hh++) {
            ncnn::Mat mc = mask.channel(i * H_Q + hh);
            for (int q = 0; q < l0; q++) {
                float* mr = mc.row(q);
                if (q < m) mr[m] = NEG_HAYAI;
                for (int j = M; j < m; j++) mr[j] += NEG_HAYAI;
            }
        }
        for (int q = 0; q < l0; q++) {
            float* c = cs.row(i * l0 + q);
            float* s = sn.row(i * l0 + q);
            if (q < M) {
                const float yy = (float)(q / wo[i]), xx = (float)(q % wo[i]);
                for (int k = 0; k < H_AXIS / 2; k++) {
                    c[k] = std::cos(yy * f[k]);
                    s[k] = std::sin(yy * f[k]);
                    c[16 + k] = std::cos(xx * f[k]);
                    s[16 + k] = std::sin(xx * f[k]);
                }
            } else {
                for (int k = 0; k < H_AXIS; k++) {
                    c[k] = 1.f;
                    s[k] = 0.f;
                }
            }
        }
    }
    std::vector<ncnn::VkMat> past(2 * H_LAYERS);
    std::vector<int> nxt;
    {
        ncnn::Net* xc = h->xcat_net(2 * b);
        if (!xc) {
            set_err(err, errlen, "concat net");
            return BN_ERR_LOAD;
        }
        ncnn::VkCompute cmd(h->vkdev);
        ncnn::Extractor ex = h->ex(*xc);
        for (int i = 0; i < b; i++) {
            ex.input(("in" + std::to_string(2 * i)).c_str(), vis[i]);
            ex.input(("in" + std::to_string(2 * i + 1)).c_str(), tails[i]);
        }
        ncnn::VkMat x;
        if (ex.extract("x", x, cmd) != 0) {
            set_err(err, errlen, "concat failed");
            return BN_ERR_RUN;
        }
        ncnn::Extractor ep = h->ex(*h->prefill);
        ep.input("x", x);
        ep.input("mask", mask);
        ep.input("cos", cs);
        ep.input("sin", sn);
        for (int j = 0; j < H_LAYERS; j++)
            if (ep.extract(("k" + std::to_string(j)).c_str(), past[2 * j], cmd) != 0 ||
                ep.extract(("v" + std::to_string(j)).c_str(), past[2 * j + 1], cmd) != 0) {
                set_err(err, errlen, "prefill failed");
                return BN_ERR_RUN;
            }
        ncnn::VkMat keep;
        ncnn::Mat host;
        if (h->record_ids(ep, cmd, host, keep) != 0 || cmd.submit_and_wait() != 0) {
            set_err(err, errlen, "prefill failed");
            return BN_ERR_RUN;
        }
        nxt = h->ids_from(host, b);
    }
    vis.clear();
    h->secs[1] += now() - tp;
    // decode loop: the KV cache stays on the GPU (SDPA's kv_cache concat)
    const double ts = now();
    std::vector<std::vector<int>> toks(b);
    std::vector<char> alive(b);
    for (int i = 0; i < b; i++) {
        toks[i].push_back(nxt[i]);
        alive[i] = nxt[i] != H_EOS && nxt[i] != H_PAD;
    }
    int seqlen = l0;
    for (int stepi = 1; stepi < max_new; stepi++) {
        if (std::none_of(alive.begin(), alive.end(), [](char a) { return a; })) break;
        const int L = seqlen + 1;
        ncnn::Mat x = mat2(H_DM, b);
        for (int i = 0; i < b; i++) {
            if (nxt[i] < 0 || nxt[i] >= H_VOCAB) {
                set_err(err, errlen, "token out of range");
                return BN_ERR_RUN;
            }
            std::memcpy(x.row(i), &h->emb[(size_t)nxt[i] * H_DM], H_DM * 4);
        }
        ncnn::Mat sm(L, 1, H_Q * b, (size_t)4u);
        sm.fill(0.f);
        for (int i = 0; i < b; i++)
            for (int hh = 0; hh < H_Q; hh++) {
                float* r = sm.channel(i * H_Q + hh);
                for (int j = valid[i]; j < m; j++) r[j] = NEG_HAYAI;
            }
        ncnn::Mat c1 = mat2(H_AXIS, 1), s1 = mat2(H_AXIS, 1);
        for (int k = 0; k < H_AXIS / 2; k++) {
            const float a = (float)stepi * f[k];
            ((float*)c1.data)[k] = ((float*)c1.data)[16 + k] = std::cos(a);
            ((float*)s1.data)[k] = ((float*)s1.data)[16 + k] = std::sin(a);
        }
        ncnn::VkCompute cmd(h->vkdev);
        ncnn::Extractor es = h->ex(*h->step);
        es.input("x", x);
        es.input("mask", sm);
        es.input("cos", c1);
        es.input("sin", s1);
        for (int j = 0; j < H_LAYERS; j++) {
            es.input(("pk" + std::to_string(j)).c_str(), past[2 * j]);
            es.input(("pv" + std::to_string(j)).c_str(), past[2 * j + 1]);
        }
        std::vector<ncnn::VkMat> np(2 * H_LAYERS);
        for (int j = 0; j < H_LAYERS; j++)
            if (es.extract(("nk" + std::to_string(j)).c_str(), np[2 * j], cmd) != 0 ||
                es.extract(("nv" + std::to_string(j)).c_str(), np[2 * j + 1], cmd) != 0) {
                set_err(err, errlen, "decode step failed");
                return BN_ERR_RUN;
            }
        ncnn::VkMat keep;
        ncnn::Mat host;
        if (h->record_ids(es, cmd, host, keep) != 0 || cmd.submit_and_wait() != 0) {
            set_err(err, errlen, "decode step failed");
            return BN_ERR_RUN;
        }
        past.swap(np);
        const std::vector<int> got = h->ids_from(host, b);
        h->steps++;
        seqlen++;
        for (int i = 0; i < b; i++) {
            const int v = alive[i] ? got[i] : H_PAD;
            nxt[i] = v;
            if (alive[i]) {
                toks[i].push_back(v);
                alive[i] = v != H_EOS && v != H_PAD;
            }
        }
    }
    h->secs[3] += now() - ts;
    for (int i = 0; i < b; i++) {
        int c = 0;
        for (int v : toks[i])
            if (v != H_EOS && v != H_PAD && c < max_new) out_ids[(size_t)i * max_new + c++] = v;
        out_len[i] = c;
    }
    return BN_OK;
    BN_TRY_END
}

// ---- paddle -----------------------------------------------------------------------------

bn_paddle* bn_paddle_load(const char* dir, int gpu, uint32_t flags, const int32_t* prefix, int32_t n_prefix,
                          const int32_t* suffix, int32_t n_suffix, int32_t vocab, char* err, size_t errlen) {
    try {
        if (!prefix || !suffix || n_prefix <= 0 || n_suffix <= 0 || vocab <= 0) {
            set_err(err, errlen, "bad prompt or vocabulary size");
            return nullptr;
        }
        auto h = std::make_unique<bn_paddle>();
        if (!load_engine(dir, gpu, flags, 1536.0, err, errlen, h)) return nullptr;
        if (!layer_ok(h->vkdev, 1, h->opt)) {  // masked attention has no stock path
            set_err(err, errlen, std::string("the attention kernel failed its self-check on ") +
                                     h->vkdev->info.device_name() + " and paddle-manga has no stock fallback for it");
            return nullptr;
        }
        std::string why;
        h->vocab = vocab;
        h->emb = read_f32(h->dir + "/embeddings.f32", (size_t)vocab * P_DT, why);
        if (!h->emb.empty()) h->pos_table = read_f32(h->dir + "/pos_table.f32", (size_t)P_SIDE * P_SIDE * P_DV, why);
        if (h->pos_table.empty()) {
            set_err(err, errlen, why);
            return nullptr;
        }
        for (int i = 0; i < n_prefix; i++)
            if (prefix[i] < 0 || prefix[i] >= vocab) { set_err(err, errlen, "prompt id out of range"); return nullptr; }
        for (int i = 0; i < n_suffix; i++)
            if (suffix[i] < 0 || suffix[i] >= vocab) { set_err(err, errlen, "prompt id out of range"); return nullptr; }
        h->prefix.assign(prefix, prefix + n_prefix);
        h->suffix.assign(suffix, suffix + n_suffix);
        for (int i = 0; i < P_HD / 2; i++) h->text_inv[i] = 1.0f / std::pow(500000.0f, (float)(2 * i) / P_HD);
        return h.release();
    } catch (const std::exception& e) {
        set_err(err, errlen, std::string("internal error: ") + e.what());
        return nullptr;
    }
}

void bn_paddle_free(bn_paddle* h) { delete h; }

void bn_paddle_stats(const bn_paddle* h, double out_secs[4], int64_t* out_steps) {
    for (int i = 0; i < 4; i++) out_secs[i] = h->secs[i];
    *out_steps = h->steps;
}

int bn_paddle_generate(bn_paddle* h, int n, const int32_t* gh, const int32_t* gw, const float* const* pixels,
                       const int32_t* const* pos_idx, const float* const* pos_w, const float* const* vcos,
                       const float* const* vsin, const int32_t* caps, int32_t max_cap, int32_t* out_ids,
                       int32_t* out_len, char* err, size_t errlen) {
    if (!h || n <= 0 || max_cap <= 0) return BN_ERR_ARG;
    std::lock_guard<std::mutex> lock(h->mu);
    BN_TRY_BEGIN
    const int b = n;
    const int npre = (int)h->prefix.size(), nsuf = (int)h->suffix.size();
    // vision per crop
    const double tv = now();
    std::vector<ncnn::VkMat> vis(b);
    std::vector<int> nimg(b), hb(b), wb(b);
    for (int i = 0; i < b; i++) {
        if (gh[i] < 2 || gw[i] < 2 || gh[i] % 2 || gw[i] % 2) return BN_ERR_ARG;
        const int N = gh[i] * gw[i];
        hb[i] = gh[i] / 2;
        wb[i] = gw[i] / 2;
        const int M = hb[i] * wb[i];
        nimg[i] = M;
        ncnn::Mat pv = mat2(P_PATCH_DIM, N, pixels[i]);
        ncnn::Mat pe = mat2(P_DV, N);
        for (int r = 0; r < N; r++) {
            float* o = pe.row(r);
            for (int t = 0; t < 4; t++) {
                const int idx = pos_idx[i][r * 4 + t];
                const float wgt = pos_w[i][r * 4 + t];
                if (idx < 0 || idx >= P_SIDE * P_SIDE) return BN_ERR_ARG;
                const float* row = &h->pos_table[(size_t)idx * P_DV];
                for (int d = 0; d < P_DV; d++) o[d] += row[d] * wgt;
            }
        }
        ncnn::Mat c = mat2(P_VHALF, N, vcos[i]), s = mat2(P_VHALF, N, vsin[i]);
        ncnn::Mat sel = mat2(N, 4 * M);
        for (int bi = 0; bi < hb[i]; bi++)
            for (int bj = 0; bj < wb[i]; bj++) {
                const int t = bi * wb[i] + bj;
                const int idx[4] = {(2 * bi) * gw[i] + 2 * bj, (2 * bi) * gw[i] + 2 * bj + 1,
                                    (2 * bi + 1) * gw[i] + 2 * bj, (2 * bi + 1) * gw[i] + 2 * bj + 1};
                for (int k = 0; k < 4; k++) ((float*)sel.row(t * 4 + k))[idx[k]] = 1.f;
            }
        ncnn::VkCompute cmd(h->vkdev);
        ncnn::Extractor e = h->ex(*h->vision);
        e.input("pv", pv);
        e.input("pos", pe);
        e.input("cos", c);
        e.input("sin", s);
        e.input("sel", sel);
        if (e.extract("vis", vis[i], cmd) != 0 || cmd.submit_and_wait() != 0) {
            set_err(err, errlen, "vision failed");
            return BN_ERR_RUN;
        }
    }
    h->secs[0] += now() - tv;
    // prompt rows left-padded to S; positions as 0.5.2 (prefix, image grid, suffix)
    const double tp = now();
    int S = 0;
    for (int i = 0; i < b; i++) S = std::max(S, npre + nimg[i] + nsuf);
    std::vector<ncnn::Mat> heads(b), tails(b);
    ncnn::Mat cs = mat2(P_HD / 2, b * S), sn = mat2(P_HD / 2, b * S);
    ncnn::Mat mask(S, S, b, (size_t)4u);
    std::vector<char> valid((size_t)b * S, 0);
    std::vector<long> base(b);
    for (int i = 0; i < b; i++) {
        const int len = npre + nimg[i] + nsuf, pad = S - len;
        heads[i] = mat2(P_DT, pad + npre);
        for (int r = 0; r < pad; r++) std::memcpy(heads[i].row(r), &h->emb[(size_t)P_PAD * P_DT], P_DT * 4);
        for (int r = 0; r < npre; r++)
            std::memcpy(heads[i].row(pad + r), &h->emb[(size_t)h->prefix[r] * P_DT], P_DT * 4);
        tails[i] = mat2(P_DT, nsuf);
        for (int r = 0; r < nsuf; r++) std::memcpy(tails[i].row(r), &h->emb[(size_t)h->suffix[r] * P_DT], P_DT * 4);
        int row = i * S;
        for (int r = 0; r < pad; r++, row++) mrope(h, 0, 0, 0, cs.row(row), sn.row(row));
        long mx = 0;
        for (int r = 0; r < npre; r++, row++) {
            mrope(h, r, r, r, cs.row(row), sn.row(row));
            mx = std::max(mx, (long)r);
        }
        const long p0 = npre;
        for (int bi = 0; bi < hb[i]; bi++)
            for (int bj = 0; bj < wb[i]; bj++, row++) {
                mrope(h, p0, p0 + bi, p0 + bj, cs.row(row), sn.row(row));
                mx = std::max(mx, std::max(p0 + bi, p0 + bj));
            }
        const long nx = p0 + std::max(hb[i], wb[i]);
        for (int r = 0; r < nsuf; r++, row++) {
            mrope(h, nx + r, nx + r, nx + r, cs.row(row), sn.row(row));
            mx = std::max(mx, nx + r);
        }
        base[i] = mx + 1;
        for (int r = pad; r < S; r++) valid[(size_t)i * S + r] = 1;
        ncnn::Mat mc = mask.channel(i);
        for (int q = 0; q < S; q++) {
            float* mr = mc.row(q);
            for (int k = 0; k < S; k++) {
                const bool allow = (k <= q && valid[(size_t)i * S + k]) || k == q;
                mr[k] = allow ? 0.f : NEG_PADDLE;
            }
        }
    }
    std::vector<ncnn::VkMat> past(2 * P_LAYERS);
    std::vector<int> tok;
    {
        ncnn::Net* xc = h->xcat_net(3 * b);
        if (!xc) {
            set_err(err, errlen, "concat net");
            return BN_ERR_LOAD;
        }
        ncnn::VkCompute cmd(h->vkdev);
        ncnn::Extractor ex = h->ex(*xc);
        for (int i = 0; i < b; i++) {
            ex.input(("in" + std::to_string(3 * i)).c_str(), heads[i]);
            ex.input(("in" + std::to_string(3 * i + 1)).c_str(), vis[i]);
            ex.input(("in" + std::to_string(3 * i + 2)).c_str(), tails[i]);
        }
        ncnn::VkMat x;
        if (ex.extract("x", x, cmd) != 0) {
            set_err(err, errlen, "concat failed");
            return BN_ERR_RUN;
        }
        ncnn::Extractor ep = h->ex(*h->prefill);
        ep.input("x", x);
        ep.input("mask", mask);
        ep.input("cos", cs);
        ep.input("sin", sn);
        for (int j = 0; j < P_LAYERS; j++)
            if (ep.extract(("k" + std::to_string(j)).c_str(), past[2 * j], cmd) != 0 ||
                ep.extract(("v" + std::to_string(j)).c_str(), past[2 * j + 1], cmd) != 0) {
                set_err(err, errlen, "prefill failed");
                return BN_ERR_RUN;
            }
        ncnn::VkMat keep;
        ncnn::Mat host;
        if (h->record_ids(ep, cmd, host, keep) != 0 || cmd.submit_and_wait() != 0) {
            set_err(err, errlen, "prefill failed");
            return BN_ERR_RUN;
        }
        tok = h->ids_from(host, b);
    }
    vis.clear();
    h->secs[1] += now() - tp;
    // greedy loop with per-row caps (0.5.2's order: take, stop, feed)
    const double ts = now();
    std::vector<std::vector<int>> out(b);
    std::vector<char> done(b);
    for (int i = 0; i < b; i++) done[i] = caps[i] <= 0;
    std::vector<char> keyvalid = valid;
    int total = S;
    for (int stepi = 0; stepi < max_cap; stepi++) {
        for (int i = 0; i < b; i++) {
            if (done[i]) continue;
            if (tok[i] == P_EOS) {
                done[i] = 1;
            } else {
                out[i].push_back(tok[i]);
                if ((int)out[i].size() >= caps[i]) done[i] = 1;
            }
        }
        if (std::all_of(done.begin(), done.end(), [](char d) { return d; }) || stepi + 1 == max_cap) break;
        ncnn::Mat x = mat2(P_DT, b), c1 = mat2(P_HD / 2, b), s1 = mat2(P_HD / 2, b);
        ncnn::Mat sm(total + 1, 1, b, (size_t)4u);
        std::vector<char> kv2((size_t)b * (total + 1));
        for (int i = 0; i < b; i++) {
            const int id = done[i] ? P_PAD : tok[i];
            if (id < 0 || id >= h->vocab) {
                set_err(err, errlen, "token out of range");
                return BN_ERR_RUN;
            }
            std::memcpy(x.row(i), &h->emb[(size_t)id * P_DT], P_DT * 4);
            const long p = base[i] + stepi;
            mrope(h, p, p, p, c1.row(i), s1.row(i));
            float* r = sm.channel(i);
            for (int k = 0; k < total; k++) {
                r[k] = keyvalid[(size_t)i * total + k] ? 0.f : NEG_PADDLE;
                kv2[(size_t)i * (total + 1) + k] = keyvalid[(size_t)i * total + k];
            }
            r[total] = 0.f;
            kv2[(size_t)i * (total + 1) + total] = 1;
        }
        keyvalid.swap(kv2);
        total++;
        ncnn::VkCompute cmd(h->vkdev);
        ncnn::Extractor es = h->ex(*h->step);
        es.input("x", x);
        es.input("mask", sm);
        es.input("cos", c1);
        es.input("sin", s1);
        for (int j = 0; j < P_LAYERS; j++) {
            es.input(("pk" + std::to_string(j)).c_str(), past[2 * j]);
            es.input(("pv" + std::to_string(j)).c_str(), past[2 * j + 1]);
        }
        std::vector<ncnn::VkMat> np(2 * P_LAYERS);
        for (int j = 0; j < P_LAYERS; j++)
            if (es.extract(("nk" + std::to_string(j)).c_str(), np[2 * j], cmd) != 0 ||
                es.extract(("nv" + std::to_string(j)).c_str(), np[2 * j + 1], cmd) != 0) {
                set_err(err, errlen, "decode step failed");
                return BN_ERR_RUN;
            }
        ncnn::VkMat keep;
        ncnn::Mat host;
        if (h->record_ids(es, cmd, host, keep) != 0 || cmd.submit_and_wait() != 0) {
            set_err(err, errlen, "decode step failed");
            return BN_ERR_RUN;
        }
        past.swap(np);
        tok = h->ids_from(host, b);
        h->steps++;
    }
    h->secs[3] += now() - ts;
    for (int i = 0; i < b; i++) {
        const int c = std::min((int)out[i].size(), std::min((int)caps[i], (int)max_cap));
        for (int k = 0; k < c; k++) out_ids[(size_t)i * max_cap + k] = out[i][k];
        out_len[i] = c;
    }
    return BN_OK;
    BN_TRY_END
}

}  // extern "C"
