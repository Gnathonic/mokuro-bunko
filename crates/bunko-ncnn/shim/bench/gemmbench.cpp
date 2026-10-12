// gemmbench <sgemm.comp> M K N [TM TN]: the shim's tiled GEMM vs ncnn's Gemm layer on Vulkan device 0.
// Build: g++ -O2 -std=c++17 -I<ncnn>/include/ncnn gemmbench.cpp -o gemmbench -L<ncnn>/lib -lncnn -lglslang -lSPIRV \
//   -lMachineIndependent -lGenericCodeGen -lOSDependent -lglslang-default-resource-limits -fopenmp -ldl -lpthread
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>
#include "gpu.h"
#include "layer.h"
#include "command.h"
#include "pipeline.h"
#include "net.h"
using namespace ncnn;
static double now() { return std::chrono::duration<double>(std::chrono::steady_clock::now().time_since_epoch()).count(); }
int main(int argc, char** argv) {
    std::string src_path = argv[1];
    int M = std::atoi(argv[2]), K = std::atoi(argv[3]), N = std::atoi(argv[4]);
    int tm = argc > 5 ? std::atoi(argv[5]) : 64, tn = argc > 6 ? std::atoi(argv[6]) : 64;
    std::ifstream f(src_path); std::stringstream ss; ss << f.rdbuf();
    std::string body = ss.str(); size_t nl = body.find("\n"); std::string src = body.substr(0, nl + 1) + "#define TM " + std::to_string(tm) + "\n#define TN " + std::to_string(tn) + "\n" + body.substr(nl + 1);
    VulkanDevice* vkdev = get_gpu_device(0);
    Option opt; opt.use_vulkan_compute = true; opt.use_fp16_packed = opt.use_fp16_storage = opt.use_fp16_arithmetic = false; opt.use_bf16_storage = false;
    opt.blob_vkallocator = vkdev->acquire_blob_allocator(); opt.workspace_vkallocator = opt.blob_vkallocator; opt.staging_vkallocator = vkdev->acquire_staging_allocator();
    opt.use_packing_layout = false;
    std::vector<uint32_t> spirv;
    if (compile_spirv_module(src.c_str(), opt, spirv) != 0) { std::fprintf(stderr, "compile failed\n"); return 1; }
    Pipeline pipe(vkdev); pipe.set_local_size_xyz(16, 16, 1);
    if (pipe.create(spirv.data(), spirv.size() * 4, std::vector<vk_specialization_type>()) != 0) { std::fprintf(stderr, "pipeline failed\n"); return 1; }
    Mat a(K, M), b(K, N), c(N);
    srand(1);
    for (int i = 0; i < M * K; i++) ((float*)a.data)[i] = (rand() % 2001 - 1000) / 1000.f;
    for (int i = 0; i < N * K; i++) ((float*)b.data)[i] = (rand() % 2001 - 1000) / 1000.f;
    for (int i = 0; i < N; i++) ((float*)c.data)[i] = (rand() % 2001 - 1000) / 1000.f;
    VkMat va, vb, vc, vo;
    { VkCompute cmd(vkdev); cmd.record_clone(a, va, opt); cmd.record_clone(b, vb, opt); cmd.record_clone(c, vc, opt); cmd.submit_and_wait(); }
    vo.create(N, M, 4u, 1, opt.blob_vkallocator);
    std::vector<vk_constant_type> consts(3); consts[0].i = M; consts[1].i = N; consts[2].i = K;
    VkMat disp; disp.w = (N + tn - 1) / tn * 16; disp.h = (M + tm - 1) / tm * 16; disp.c = 1;
    int iters = 50; double best = 1e9;
    for (int rep = 0; rep < 3; rep++) {
        VkCompute cmd(vkdev);
        for (int i = 0; i < iters; i++) cmd.record_pipeline(&pipe, {va, vb, vc, vo}, consts, disp);
        double t = now(); cmd.submit_and_wait(); best = std::min(best, (now() - t) / iters);
    }
    Mat out; { VkCompute cmd(vkdev); cmd.record_clone(vo, out, opt); cmd.submit_and_wait(); }
    // check a few entries
    double maxerr = 0;
    for (int s = 0; s < 64; s++) { int i = (s * 7919) % M, j = (s * 104729) % N; double ref = ((float*)c.data)[j];
        for (int k = 0; k < K; k++) ref += (double)((float*)a.data)[i * K + k] * ((float*)b.data)[j * K + k];
        maxerr = std::max(maxerr, std::fabs(ref - ((float*)out.data)[i * N + j])); }
    // ncnn Gemm layer for comparison
    Layer* g = create_layer_vulkan("Gemm"); g->vkdev = vkdev;
    ParamDict pd; pd.set(3, 1); pd.set(5, 1); pd.set(6, 1); pd.set(8, N); pd.set(9, K); pd.set(10, 4);
    g->load_param(pd);
    Mat wts[2] = {b, c}; Mat wb = b.clone(); Mat cb = c.reshape(N, 1).clone();
    struct MB : public ModelBin { Mat w, c; int i = 0; Mat load(int, int) const override { return Mat(); }
        Mat load(int w_, int h_, int) const override { return const_cast<MB*>(this)->i++ == 0 ? w : c; } } mb; mb.w = wb; mb.c = cb;
    g->load_model(mb); g->create_pipeline(opt);
    { VkTransfer up(vkdev); g->upload_model(up, opt); up.submit_and_wait(); }
    double bestg = 1e9; VkMat go;
    for (int rep = 0; rep < 3; rep++) {
        VkCompute cmd(vkdev);
        double t = now();
        for (int i = 0; i < iters; i++) { std::vector<VkMat> in{va}, outv(1); g->forward(in, outv, cmd, opt); go = outv[0]; }
        cmd.submit_and_wait(); bestg = std::min(bestg, (now() - t) / iters);
    }
    double fl = 2.0 * M * N * K;
    std::printf("%s M=%d K=%d N=%d tile=%dx%d custom %.0f us %.2f TFLOPS (maxerr %.2e) | ncnn Gemm %.0f us %.2f TFLOPS\n",
                vkdev->info.device_name(), M, K, N, tm, tn, best * 1e6, fl / best / 1e12, maxerr, bestg * 1e6, fl / bestg / 1e12);
    return 0;
}
