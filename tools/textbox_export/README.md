# textbox_export: weightless graphs for the text-box recognizers

Makes the ONNX graph Bunko runs nar-small's `r16` model with
(`muscgab/manga-ocr-nar-small` @ `8cc14c152ccb21da3aa88fa51a92766537c12da2`), and checks
it. The graph holds **no trained weights**: its 181 weight entries point at byte ranges
of the author's unmodified `model.safetensors`, which Bunko downloads from the author's
repository and hands to ONNX Runtime from memory. Only the graph is published, as the
pre-release `textbox-graphs-v1` (docs/rust-port/MODELS.md §2.3). Dev-only: nothing here
runs on users' machines.

| file | what |
|---|---|
| `export_nar.py` | the export: the author's `model.py` at the pin (Apache-2.0) with `config.json`'s RMSNorm eps values, `prepare()` inside the graph, no constant folding, no merging of equal initializers; every initializer becomes an external entry into `model.safetensors`. Refuses to write unless no weight is left inside, the I/O is `P [324,588] V [324] R C [324] H W [1] → ids [161]`, and a re-export from random parameters gives the same bytes. Writes the graph, `NOTICE.md` and the author's licence text. |
| `check_parity.py` | the parity gate: ONNX Runtime (CPU, weights from memory) against the author's PyTorch code in fp32 on the first N crops of a folder; every crop's ids must be identical. Writes the ids per crop for Bunko's ignored Rust parity test. |

```sh
# the export environment of tools/onnx_export (torch 2.14.0, onnx 1.23.1, onnxruntime
# 1.30.0, huggingface-hub, safetensors, pillow); the pinned snapshot is fetched into the
# Hugging Face cache on the first run (HF_HUB_OFFLINE=1 afterwards)
python export_nar.py --out OUT        # prints the graph's size and sha256
python export_nar.py --out OUT2 && cmp OUT/nar-small_r16_graph.onnx OUT2/nar-small_r16_graph.onnx
python check_parity.py --graph OUT/nar-small_r16_graph.onnx --crops <JMangaBench_Read crops> \
    --n 200 --expected OUT/parity.jsonl                    # 200/200 identical
# the same crops through Bunko's preprocessing and runtime
MOKURO_NAR_PARITY_CROPS=<crops> MOKURO_NAR_PARITY_EXPECTED=OUT/parity.jsonl \
MOKURO_NAR_GRAPH=OUT/nar-small_r16_graph.onnx MOKURO_NAR_WEIGHTS=<model.safetensors> \
MOKURO_NAR_VOCAB=<nar_vocabulary_6817.json> \
  cargo test -p bunko-engines --features nc-models --release --lib -- --ignored nar_parity
```

2026-11-11: `nar-small_r16_graph.onnx`, 714,861 bytes, sha256
`d630455db967a243ca70e887db5a96866bbd6e052e56f27297495aa136159948`, the same bytes on
two runs; 200/200 identical ids in both parity checks. The sha256 and size are pinned in
`crates/bunko-ocr/src/models.rs`: a new export with other bytes is a new asset name.
