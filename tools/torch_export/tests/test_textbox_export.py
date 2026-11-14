"""The text-box exporter's pure parts (CPU only, no model files).

Run from tools/torch_export: python -m pytest tests/test_textbox_export.py -q
"""

import pytest
import torch  # noqa: F401  (the exporter's modules need it importable)

from torch_export import graphs, textbox as T


def test_roles_per_engine():
    assert graphs.ROLES["nar-small"] == ("model",)
    assert graphs.ROLES["charspot-small"] == ("dense", "tail")
    assert graphs.ROLES["animetext-blocks"] == ("detector",)
    assert graphs.ROLES["hayai-nova"] == ("vision", "prefill", "step")


def test_weight_map_strips_the_prefix():
    stored = {"a.weight": ((2, 3), "F32"), "b": ((4,), "F32")}
    m = T.authors_weight_map(["net.a.weight", "net.b"], "net.", stored, stored)
    assert m == {"net.a.weight": "a.weight", "net.b": "b"}


def test_weight_map_refuses_a_missing_key():
    with pytest.raises(SystemExit, match="net.c has no key c"):
        T.authors_weight_map(["net.c"], "net.", {}, {"c": ((1,), "F32")})


def test_weight_map_refuses_a_shape_mismatch():
    with pytest.raises(SystemExit, match="shape"):
        T.authors_weight_map(["net.a"], "net.", {"a": ((2,), "F32")}, {"a": ((3,), "F32")})


def test_embedding_is_refused():
    for k in ("TORCH_EXPORT_WIN_EMBED", "TORCH_EXPORT_CPU_EMBED"):
        with pytest.raises(SystemExit, match=k):
            T.refuse_embedding({k: "1"})
    T.refuse_embedding({})


def test_pin_refuses_another_file(tmp_path):
    p = tmp_path / "model.safetensors"
    p.write_bytes(b"x" * 10)
    with pytest.raises(SystemExit, match="217035140"):
        T.check_pin(p, T.NAR_PIN)


def test_meta():
    assert T.textbox_meta(T.NAR_PIN, 64) == {
        "io": "textbox-1",
        "source": "muscgab/manga-ocr-nar-small@8cc14c152ccb21da3aa88fa51a92766537c12da2",
        "weights_sha256": "bc7418e27d476f86bb5984b45974d69cf9f7623eff6b9b57012df2c8fc9920a3",
        "weights_size": "217035140", "max_batch": "64"}


# --- fp16 (plan 02 Task 3) ---


def test_fp16_meta():
    m = T.textbox_meta(T.NAR_PIN, 64, "fp16")
    assert m["weights_dtype"] == "fp16" and m["weights_sha256"] == T.NAR_PIN.sha256
    assert "weights_dtype" not in T.textbox_meta(T.NAR_PIN, 64)


def test_texts_gate():
    from torch_export import verify

    assert verify.texts_gate(7, 7929) and not verify.texts_gate(8, 7929)
    assert verify.texts_gate(0, 1)


def test_cast_package_compares_shapes_only():
    stored = {"a": ((2,), "F32")}
    assert T.authors_weight_map(["net.a"], "net.", stored, {"a": ((2,), "F16")}, cast="F16") == {"net.a": "a"}


def test_cast_package_still_refuses_a_wrong_shape_or_dtype():
    stored = {"a": ((2,), "F32"), "i": ((1,), "I64")}
    with pytest.raises(SystemExit, match="shape"):
        T.authors_weight_map(["net.a"], "net.", stored, {"a": ((3,), "F16")}, cast="F16")
    # a floating weight the graph wants in another dtype than the cast's
    with pytest.raises(SystemExit, match="dtype F32 in the graph"):
        T.authors_weight_map(["net.a"], "net.", stored, {"a": ((2,), "F32")}, cast="F16")
    # the runtime casts floating tensors only: an integer one is bound as stored
    with pytest.raises(SystemExit, match="dtype F16 in the graph"):
        T.authors_weight_map(["net.i"], "net.", stored, {"i": ((1,), "F16")}, cast="F16")
    assert T.authors_weight_map(["net.i"], "net.", stored, {"i": ((1,), "I64")}, cast="F16") == {"net.i": "i"}


def test_fp16_compares_texts():
    from torch_export import verify

    chars = ["a", "b", "c"]  # end id 3
    assert verify.text_of([0, 1, 3, 2], chars) == "ab"
    assert verify.text_of([2, 2], chars) == "cc"

# --- verify and manifest (Task 3) ---

from torch_export import manifest, verify  # noqa: E402


def _package(path, role, size=2):
    """A .pt2 as the exporter writes it: a stored zip with one top folder ``<role>/``."""
    import zipfile

    with zipfile.ZipFile(path, "w") as z:
        z.writestr(f"{role}/data/aotinductor/model/x.wrapper.so", b"\0" * size)
    return path


def test_ids_compare_stops_at_the_end_id():
    assert verify.same_ids([5, 6817, 3], [5, 6817, 9], 6817)
    assert not verify.same_ids([5, 4, 6817], [5, 6817, 4], 6817)
    assert verify.same_ids([1, 2], [1, 2], 6817)


def test_textbox_manifest_rows(tmp_path):
    d = tmp_path / "nar-small" / "fp32" / "linux-rocm-gfx1201"
    d.mkdir(parents=True)
    _package(d / "model.pt2", "model")
    (d / "build.json").write_text('{"graphs": {"model": {"file": "model.pt2"}}}')
    m = manifest.build_textbox_manifest(tmp_path, "textbox-torch-v1", ("nar-small",),
                                        {"nar-small": T.NAR_PIN}, "Apache-2.0")
    assert m["release"] == "textbox-torch-v1"
    [row] = m["files"]
    assert row["id"] == "torch/nar-small/fp32/linux-rocm-gfx1201/model.pt2"
    assert row["role"] == "model" and "requires" not in row
    assert row["licence"] == "Apache-2.0"
    assert row["sources"] == [{"repo": "muscgab/manga-ocr-nar-small", "revision": "8cc14c152ccb21da3aa88fa51a92766537c12da2"}]


def test_detector_manifest_rows(tmp_path):
    # the same builder writes detector-torch-v1 (plan 04): its own pin and licence
    import types

    d = tmp_path / "animetext-blocks" / "fp32" / "linux-cuda-sm_80"
    d.mkdir(parents=True)
    _package(d / "detector.pt2", "detector")
    (d / "build.json").write_text('{"graphs": {"detector": {"file": "detector.pt2"}}}')
    pin = types.SimpleNamespace(repo="deepghs/AnimeText_yolo", revision="a180c191bfdb9f0e31b57e7de567e7b6bac50f84")
    m = manifest.build_textbox_manifest(tmp_path, "detector-torch-v1", ("animetext-blocks",),
                                        {"animetext-blocks": pin}, "AGPL-3.0")
    [row] = m["files"]
    assert m["release"] == "detector-torch-v1"
    assert row["file"] == "animetext-blocks_fp32_linux-cuda-sm_80_detector.pt2"
    assert row["licence"] == "AGPL-3.0"
    assert row["sources"] == [{"repo": "deepghs/AnimeText_yolo", "revision": "a180c191bfdb9f0e31b57e7de567e7b6bac50f84"}]
    with pytest.raises(SystemExit, match="animetext-blocks"):
        manifest.build_textbox_manifest(tmp_path, "detector-torch-v1", ("animetext-blocks",), {}, "AGPL-3.0")


def test_weights_inside_is_refused(tmp_path):
    import zipfile

    def pkg(name, size):
        p = tmp_path / name
        with zipfile.ZipFile(p, "w") as z:
            z.writestr("m/data/aotinductor/model/x.wrapper.so", b"\0" * size)
        return p

    assert not verify.weights_inside(pkg("thin.pt2", 40), 1000)
    assert verify.weights_inside(pkg("fat.pt2", 300), 1000)
    with pytest.raises(SystemExit, match="fat.pt2"):
        verify.refuse_embedded(tmp_path, lambda engine: 1000)
    (tmp_path / "fat.pt2").unlink()
    pkg("nar_embed.pt2", 10)  # the spike's naming: refused whatever its size
    with pytest.raises(SystemExit, match="nar_embed.pt2"):
        verify.refuse_embedded(tmp_path, lambda engine: 1000)


def test_base_manifest_leaves_text_box_packages_out(tmp_path):
    # build and manifest share the default out dir: a nar-small build there must not break
    # the hayai/paddle release manifest, nor land in it (it has a release of its own)
    import json

    from torch_export.build import sha256

    nar = tmp_path / "nar-small" / "fp32" / "linux-rocm-gfx1201"
    nar.mkdir(parents=True)
    _package(nar / "model.pt2", "model")
    (nar / "build.json").write_text(json.dumps({
        "engine": "nar-small", "precision": "fp32", "target": "linux-rocm-gfx1201", "io": "textbox-1",
        "tool": "0.1.0", "torch": "2.13.0+rocm7.1",
        "graphs": {"model": {"file": "model.pt2", "sha256": sha256(nar / "model.pt2")}},
        "authors_weights": {"repo": T.NAR_PIN.repo, "revision": T.NAR_PIN.revision}}))
    hayai = tmp_path / "hayai-nova" / "fp32" / "linux-rocm-gfx1201"
    hayai.mkdir(parents=True)
    _package(hayai / "vision.pt2", "vision")
    (hayai / "build.json").write_text(json.dumps({
        "engine": "hayai-nova", "precision": "fp32", "target": "linux-rocm-gfx1201", "io": 1,
        "tool": "0.1.0", "torch": "2.13.0+rocm7.1",
        "graphs": {"vision": {"file": "vision.pt2", "sha256": sha256(hayai / "vision.pt2")}}}))
    m = manifest.build_manifest(tmp_path, "torch-models-v1")
    assert [f["engine"] for f in m["files"]] == ["hayai-nova"]


# --- the fp16 gate's scripts and staging (plan 02 Task 7) ---

from torch_export import gate  # noqa: E402

FP16_META = {"bunko.io": "textbox-1", "bunko.weights_sha256": T.NAR_PIN.sha256, "bunko.weights_dtype": "fp16",
             "bunko.weights": '{"net.a": "a", "net.b": "b"}'}


def _meta_package(path, role, meta, size=2):
    """A .pt2 whose AOTI metadata json carries ``meta`` (what the loader's get_metadata reads)."""
    import json
    import zipfile

    with zipfile.ZipFile(path, "w") as z:
        z.writestr(f"{role}/data/aotinductor/model/x.wrapper.so", b"\0" * size)
        z.writestr(f"{role}/data/aotinductor/model/x.wrapper_metadata.json", json.dumps(meta))
    return path


def _build(root, prec, meta, target="linux-rocm-gfx1201", constants=2):
    """``root/nar-small/<prec>/<target>/`` as the exporter leaves it: model.pt2 and build.json."""
    import json

    from torch_export.build import sha256

    d = root / "nar-small" / prec / target
    d.mkdir(parents=True)
    p = _meta_package(d / "model.pt2", "model", meta)
    graph = {"file": "model.pt2", "constants": constants, "sha256": sha256(p)}
    (d / "build.json").write_text(json.dumps({"engine": "nar-small", "precision": prec, "target": target,
                                              "graphs": {"model": graph}}))
    return d


def test_package_meta_reads_the_bunko_keys(tmp_path):
    p = _meta_package(tmp_path / "model.pt2", "model", FP16_META)
    assert verify.package_meta(p)["bunko.weights_dtype"] == "fp16"
    assert verify.package_meta(_package(tmp_path / "bare.pt2", "model")) == {}


def test_staging_refuses_a_package_whose_cast_disagrees_with_its_precision(tmp_path):
    no_cast = {k: v for k, v in FP16_META.items() if k != "bunko.weights_dtype"}
    cases = [("fp16", no_cast, "bunko.weights_dtype is None, a fp16 package says 'fp16'"),
             ("fp32", FP16_META, "bunko.weights_dtype is 'fp16', a fp32 package says None"),
             ("fp16", {**FP16_META, "bunko.weights_sha256": "0" * 64}, "bunko.weights_sha256")]
    for i, (prec, meta, err) in enumerate(cases):
        root = tmp_path / str(i)
        _build(root, prec, meta)
        with pytest.raises(SystemExit, match=err):
            manifest.build_textbox_manifest(root, "textbox-torch-v1", ("nar-small",), {"nar-small": T.NAR_PIN},
                                            "Apache-2.0")
    root = tmp_path / "ok"
    _build(root, "fp16", FP16_META)
    _build(root, "fp32", no_cast)
    m = manifest.build_textbox_manifest(root, "textbox-torch-v1", ("nar-small",), {"nar-small": T.NAR_PIN},
                                        "Apache-2.0")
    assert sorted(r["precision"] for r in m["files"]) == ["fp16", "fp32"]


def _reads(path, texts):
    import json

    path.write_text("".join(json.dumps({"file": f, "ids": [], "text": t}, ensure_ascii=False) + "\n"
                            for f, t in texts.items()), encoding="utf-8")
    return path


def test_compare_reads_counts_differing_texts(tmp_path):
    a = _reads(tmp_path / "a.jsonl", {f"{i}.png": "あ" for i in range(1000)})
    b = _reads(tmp_path / "b.jsonl", {f"{i}.png": ("い" if i == 7 else "あ") for i in range(1000)})
    r = gate.compare_reads(a, b)
    assert r == {"crops": 1000, "differ": ["7.png"], "limit": 1, "passed": True}
    c = _reads(tmp_path / "c.jsonl", {f"{i}.png": ("い" if i < 2 else "あ") for i in range(1000)})
    assert not gate.compare_reads(a, c)["passed"]
    with pytest.raises(SystemExit, match="1 crops only in"):
        gate.compare_reads(a, _reads(tmp_path / "d.jsonl", {f"{i}.png": "あ" for i in range(999)}))
    # within: only the first file's crops are compared, the second may have more
    assert gate.compare_reads(tmp_path / "d.jsonl", a, within=True)["crops"] == 999
    with pytest.raises(SystemExit, match="1 crops only in"):
        gate.compare_reads(a, tmp_path / "d.jsonl", within=True)


def test_block_preds_join_texts_in_block_order(tmp_path):
    import json

    reads = _reads(tmp_path / "r.jsonl", {"k1__b0.png": "上", "k1__b1.png": "下", "k2__b0.png": "x"})
    blocks = tmp_path / "blocks.jsonl"
    blocks.write_text('{"key": "k1", "crops": ["k1__b0.png", "k1__b1.png"]}\n{"key": "k0", "crops": []}\n'
                      '{"key": "k2", "crops": ["k2__b0.png"]}\n')
    out = tmp_path / "preds.jsonl"
    gate.block_preds(reads, blocks, out)
    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert rows == [{"key": "k1", "pred": "上下", "n_blocks": 2}, {"key": "k0", "pred": "", "n_blocks": 0},
                    {"key": "k2", "pred": "x", "n_blocks": 1}]
    blocks.write_text('{"key": "k3", "crops": ["k3__b0.png"]}\n')
    with pytest.raises(SystemExit, match="k3__b0.png"):
        gate.block_preds(reads, blocks, out)


def _gate_inputs(tmp_path, meta=FP16_META):
    out = tmp_path / "build"
    _build(out, "fp16", meta)
    crops = tmp_path / "crops"
    crops.mkdir()
    for f in ("a.png", "b.png"):
        (crops / f).write_bytes(f.encode())
    (crops / "notes.txt").write_text("not a crop")
    ref = _reads(tmp_path / "nar-fp16-expected.jsonl", {"a.png": "あ", "b.png": "い"})
    fp32 = _reads(tmp_path / "nar-fp32-expected.jsonl", {"a.png": "あ", "b.png": "い", "z.png": "not staged"})
    return out, crops, ref, fp32


def test_gate_bundle_is_self_contained_and_summed(tmp_path):
    import hashlib
    import json
    import os

    out, crops, ref, fp32 = _gate_inputs(tmp_path)
    dest = tmp_path / "bundle"
    b = gate.stage_bundle(out, "nar-small", "fp16", "linux-rocm-gfx1201", crops, ref, fp32, "HEAD", dest)
    assert b == dest
    info = json.loads((dest / "bundle.json").read_text())
    assert info["engine"] == "nar-small" and info["precision"] == "fp16" and info["target"] == "linux-rocm-gfx1201"
    assert info["reference"] == "expected/nar-fp16-expected.jsonl" and info["fp32"] == "expected/nar-fp32-expected.jsonl"
    assert info["crops"] == 2 and info["batches"] == [1, 8, 32] and len(info["commit"]) == 40
    assert (dest / "package/nar-small/fp16/linux-rocm-gfx1201/model.pt2").is_file()
    assert sorted(p.name for p in (dest / "crops").iterdir()) == ["a.png", "b.png"]
    assert (dest / "src/tools/torch_export/torch_export/verify.py").is_file()
    assert (dest / "src/tools/textbox_export/check_parity.py").is_file()
    assert os.access(dest / "run.sh", os.X_OK)
    sums = dict(reversed(line.split("  ", 1)) for line in (dest / "SHA256SUMS").read_text().splitlines())
    files = sorted(str(p.relative_to(dest)) for p in dest.rglob("*") if p.is_file() and p.name != "SHA256SUMS")
    assert sorted(sums) == files
    for rel, digest in sums.items():
        assert hashlib.sha256((dest / rel).read_bytes()).hexdigest() == digest, rel
    with pytest.raises(SystemExit, match="exists"):
        gate.stage_bundle(out, "nar-small", "fp16", "linux-rocm-gfx1201", crops, ref, fp32, "HEAD", dest)


def test_gate_bundle_refuses_what_the_gate_cannot_use(tmp_path):
    out, crops, ref, fp32 = _gate_inputs(tmp_path)
    args = ("nar-small", "fp16", "linux-rocm-gfx1201", crops)
    _reads(fp32, {"a.png": "あ"})
    with pytest.raises(SystemExit, match="no row for 1 crops, first .'b.png'"):
        gate.stage_bundle(out, *args, ref, fp32, "HEAD", tmp_path / "b1")
    _reads(fp32, {"a.png": "あ", "b.png": "い"})
    with pytest.raises(SystemExit, match="not a commit"):
        gate.stage_bundle(out, *args, ref, fp32, "no-such-ref", tmp_path / "b2")
    out2, *_ = _gate_inputs(tmp_path / "t2", {k: v for k, v in FP16_META.items() if k != "bunko.weights_dtype"})
    with pytest.raises(SystemExit, match="bunko.weights_dtype"):
        gate.stage_bundle(out2, *args, ref, fp32, "HEAD", tmp_path / "b3")
    out3 = tmp_path / "t3"
    _build(out3, "fp16", FP16_META, constants=3)
    with pytest.raises(SystemExit, match="3 constants, 2 in bunko.weights"):
        gate.stage_bundle(out3, *args, ref, fp32, "HEAD", tmp_path / "b4")
    assert not any((tmp_path / f"b{i}").exists() for i in range(1, 5))


def test_gate_run_checks_reference_fp32_and_batch_pairs(tmp_path):
    import json

    out, crops, ref, fp32 = _gate_inputs(tmp_path)
    dest = gate.stage_bundle(out, "nar-small", "fp16", "linux-rocm-gfx1201", crops, ref, fp32, "HEAD",
                             tmp_path / "bundle")
    texts = {1: {"a.png": "あ", "b.png": "い"}, 8: {"a.png": "あ", "b.png": "い"}, 32: {"a.png": "あ", "b.png": "い"}}
    calls = []

    def fake_verify(bundle, info, batch, expected, reads):
        calls.append((batch, expected.name))
        _reads(reads, texts[batch])
        return {"crops": 2, "identical": 2, "differ": [], "batch": batch, "passed": True, "crops_per_s": 10.0 * batch}

    s = gate.run_gate(dest, fake_verify)
    assert calls == [(1, "nar-fp16-expected.jsonl"), (8, "nar-fp16-expected.jsonl"), (32, "nar-fp16-expected.jsonl")]
    assert s["passed"] and s["checks"]["reference"]["passed"] and s["checks"]["fp32"]["passed"]
    assert sorted(s["checks"]["batches"]) == ["b1-b32", "b1-b8", "b8-b32"]
    assert s["crops_per_s"] == {"1": 10.0, "8": 80.0, "32": 320.0}
    assert json.loads((dest / "results/summary.json").read_text())["passed"]
    # with 2 crops the limit is 0: one differing text at b8 fails two batch pairs
    texts[8] = {"a.png": "あ", "b.png": "う"}
    s = gate.run_gate(dest, fake_verify)
    assert not s["passed"]
    assert {k: v["passed"] for k, v in s["checks"]["batches"].items()} == {"b1-b8": False, "b1-b32": True,
                                                                          "b8-b32": False}
