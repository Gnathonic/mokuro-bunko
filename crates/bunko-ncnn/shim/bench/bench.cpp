// Bench: read dumped crop inputs (crops.bin from ncnn_export.dump), run hayai on
// ncnn in batches, write the ids as JSON lines, print stage timings.
//   bench <model dir> <crops.bin> <out.jsonl> [--gpu N] [--fp16] [--batch 16] [--repeat R]
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include "bunko_ncnn.h"

int main(int argc, char** argv) {
    if (argc < 4) { std::fprintf(stderr, "usage: bench dir crops.bin out.jsonl [--gpu N] [--fp16] [--batch B] [--repeat R]\n"); return 2; }
    int gpu = 0, batch = 16, repeat = 1;
    uint32_t flags = 0;
    for (int i = 4; i < argc; i++) {
        std::string a = argv[i];
        if (a == "--gpu") gpu = std::atoi(argv[++i]);
        else if (a == "--fp16") flags |= BN_FLAG_FP16_STORAGE;
        else if (a == "--batch") batch = std::atoi(argv[++i]);
        else if (a == "--repeat") repeat = std::atoi(argv[++i]);
    }
    FILE* f = std::fopen(argv[2], "rb");
    if (!f) return 3;
    int32_t n;
    std::fread(&n, 4, 1, f);
    std::vector<int32_t> hp(n), wp(n);
    std::vector<std::vector<float>> pv(n), pos(n);
    for (int i = 0; i < n; i++) {
        std::fread(&hp[i], 4, 1, f);
        std::fread(&wp[i], 4, 1, f);
        size_t P = (size_t)hp[i] * wp[i] * 768;
        pv[i].resize(P);
        pos[i].resize(P);
        std::fread(pv[i].data(), 4, P, f);
        std::fread(pos[i].data(), 4, P, f);
    }
    std::fclose(f);
    char err[512] = {0}, name[256] = {0};
    if (gpu >= 0) { bn_gpu_name(gpu, name, sizeof name); }
    auto t0 = std::chrono::steady_clock::now();
    bn_hayai* h = bn_hayai_load(argv[1], gpu, flags, err, sizeof err);
    if (!h) { std::fprintf(stderr, "load: %s\n", err); return 4; }
    double load_s = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
    const int MAXN = 96;
    std::vector<int32_t> ids((size_t)n * MAXN), len(n);
    // warm-up batch (pipeline creation, allocator growth)
    {
        int b = std::min(batch, n);
        std::vector<const float*> a(b), c(b);
        for (int i = 0; i < b; i++) { a[i] = pv[i].data(); c[i] = pos[i].data(); }
        if (bn_hayai_generate(h, b, hp.data(), wp.data(), a.data(), c.data(), MAXN, ids.data(), len.data(), err, sizeof err) != 0) { std::fprintf(stderr, "run: %s\n", err); return 5; }
    }
    double s0[4]; int64_t st0;
    bn_hayai_stats(h, s0, &st0);
    auto t1 = std::chrono::steady_clock::now();
    for (int r = 0; r < repeat; r++)
        for (int s = 0; s < n; s += batch) {
            int b = std::min(batch, n - s);
            std::vector<const float*> a(b), c(b);
            for (int i = 0; i < b; i++) { a[i] = pv[s + i].data(); c[i] = pos[s + i].data(); }
            if (bn_hayai_generate(h, b, hp.data() + s, wp.data() + s, a.data(), c.data(), MAXN, ids.data() + (size_t)s * MAXN, len.data() + s, err, sizeof err) != 0) { std::fprintf(stderr, "run: %s\n", err); return 5; }
        }
    double secs = std::chrono::duration<double>(std::chrono::steady_clock::now() - t1).count();
    double s1[4]; int64_t st1;
    bn_hayai_stats(h, s1, &st1);
    FILE* o = std::fopen(argv[3], "w");
    for (int i = 0; i < n; i++) {
        std::fprintf(o, "[");
        for (int k = 0; k < len[i]; k++) std::fprintf(o, k ? ",%d" : "%d", ids[(size_t)i * MAXN + k]);
        std::fprintf(o, "]\n");
    }
    std::fclose(o);
    std::printf("{\"device\":\"%s\",\"fp16\":%d,\"batch\":%d,\"crops\":%d,\"crops_per_s\":%.2f,\"load_s\":%.2f,"
                "\"vision_s\":%.3f,\"prefill_s\":%.3f,\"concat_s\":%.3f,\"decode_s\":%.3f,\"steps\":%lld,\"ms_per_step\":%.2f}\n",
                name, (flags & BN_FLAG_FP16_STORAGE) ? 1 : 0, batch, n * repeat, n * repeat / secs, load_s,
                s1[0] - s0[0], s1[1] - s0[1], s1[2] - s0[2], s1[3] - s0[3], (long long)(st1 - st0),
                1000.0 * (s1[3] - s0[3]) / std::max<int64_t>(1, st1 - st0));
    bn_hayai_free(h);
    return 0;
}
