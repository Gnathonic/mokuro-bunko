# webgpu_export: the WebGPU backend's graphs

Builds and verifies the ONNX graphs the Vulkan pack's WebGPU runtime runs
(docs/rust-port/VULKAN.md): hand-written graphs with the same math as the exported ones
(`tools/onnx_export`), shaped for ONNX Runtime's WebGPU execution provider.

| engine | files | what |
|---|---|---|
| hayai-nova | `hayai-nova_webgpu_vision_fp32.onnx` | SigLIP2 + projector; fused residual norms, FastGelu, the 1/8 scale folded into the query weights (exact) |
| | `hayai-nova_webgpu_decoder_fp32.onnx` | the shared decoder (`graph.py`): prefill and every step, KV cache on the GPU |
| paddle-manga | `paddle-manga_webgpu_{embed,layer,qkv,attn,post,tail}_fp32.onnx` | the vision tower layer by layer, weights as inputs (bounded VRAM on big crops) |
| | `paddle-manga_webgpu_weights_fp32.onnx(.data)` | the tower's weights, uploaded to the GPU once by running it |
| | `paddle-manga_webgpu_decoder_fp32.onnx` | the shared decoder |
| | `paddle-manga_webgpu_vision.json` | layers, whole-layer limit and attention chunk |

Why the shapes (measured, docs/rust-port/VULKAN.md): the EP runs a 16-row
GEMM on a handful of workgroups (split-K up to 64 rows, deterministic), its ArgMax uses one
thread per row (two-stage argmax), and it frees a run's buffers only when the run ends
(paddle's vision runs layer by layer, the prompt is prefilled in chunks). Its fused
MultiHeadAttention is faster but not fp32-exact, so attention stays decomposed.

```sh
# export environment (torch, transformers, peft, onnx) + the pinned snapshots in the HF cache
HF_HUB_OFFLINE=1 python -m webgpu_export build --engine all --out OUT
# onnxruntime (CPU) or onnxruntime-webgpu; spike crops in ~/.cache/mokuro-bunko-demo/onnx-spike
python -m webgpu_export verify --engine hayai  --out OUT --models MODELS_V1 --ep webgpu   # 220/220
python -m webgpu_export verify --engine paddle --out OUT --models MODELS_V1 --ep webgpu   # 100/100
```

`build` prints each file's size and sha256; the bytes are reproducible (sequential node
names, no timestamps). `host.py` is the Python twin of the Rust host (`crates/bunko-vlm`:
`gpudec.rs`, `hayai/gpu.rs`, `paddle/gpu.rs`).
