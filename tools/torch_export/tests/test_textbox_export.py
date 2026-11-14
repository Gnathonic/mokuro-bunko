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


# --- charspot-small (plan 03 Task 3) ---


def test_charspot_pin_and_roles():
    assert T.CHARSPOT_PIN.prefix == {"dense": "dense.", "tail": "tail."}
    assert T.PINS["charspot-small"] is T.CHARSPOT_PIN
    assert graphs.BUILDERS["charspot-small"] is not None
    assert verify.pinned_size("charspot-small") == 43_860_012
    with pytest.raises(SystemExit, match="fp32"):
        T.charspot_small("fp16", torch.device("cpu"), 0)


def test_same_chars():
    a = {"text": "あい", "chars": [["あ", 0, 0, 10, 10, .9], ["い", 0, 10, 10, 20, .8]]}
    b = {"text": "あい", "chars": [["あ", .4, 0, 10, 10, .9], ["い", 0, 10, 10.3, 20, .7]]}
    assert verify.same_chars(a, b)
    b["chars"][0][1] = 0.6
    assert not verify.same_chars(a, b)
    # same text, another character count or order: not the same
    assert not verify.same_chars(a, {"text": "あい", "chars": a["chars"][:1]})
    assert not verify.same_chars(a, {"text": "あい", "chars": a["chars"][::-1]})
    assert not verify.same_chars(a, {"text": "あう", "chars": a["chars"]})
    assert verify.same_chars({"text": "", "chars": []}, {"text": "", "chars": []})


def test_charspot_manifest_rows(tmp_path):
    d = tmp_path / "charspot-small" / "fp32" / "linux-rocm-gfx1201"
    d.mkdir(parents=True)
    for role in ("dense", "tail"):
        _package(d / f"{role}.pt2", role)
    (d / "build.json").write_text('{"graphs": {"dense": {"file": "dense.pt2"}, "tail": {"file": "tail.pt2"}}}')
    m = manifest.build_textbox_manifest(tmp_path, "textbox-torch-v1", ("charspot-small",),
                                        {"charspot-small": T.CHARSPOT_PIN}, "Apache-2.0")
    assert [r["id"] for r in m["files"]] == ["torch/charspot-small/fp32/linux-rocm-gfx1201/dense.pt2",
                                             "torch/charspot-small/fp32/linux-rocm-gfx1201/tail.pt2"]
    assert all(r["licence"] == "Apache-2.0" and "requires" not in r for r in m["files"])
    assert m["files"][0]["sources"] == [{"repo": "muscgab/manga-ocr-charspot-small",
                                         "revision": "72ca29202673880ca9c67b1c02c107535be4472b"}]


def test_charspot_weights_inside_is_refused(tmp_path):
    import zipfile

    d = tmp_path / "charspot-small" / "fp32" / "linux-rocm-gfx1201"
    d.mkdir(parents=True)
    with zipfile.ZipFile(d / "dense.pt2", "w") as z:  # the spike's dense with weights inside: 31,187,272 B
        z.writestr("m/data/aotinductor/model/x.wrapper.so", b"\0" * (T.CHARSPOT_PIN.size // 4 + 1))
    with pytest.raises(SystemExit, match="dense.pt2"):
        verify.refuse_embedded(tmp_path, lambda engine: T.CHARSPOT_PIN.size if engine == "charspot-small" else 0)
    # and through the CLI's own size lookup
    with pytest.raises(SystemExit, match="dense.pt2"):
        verify.refuse_embedded(tmp_path, verify.pinned_size)


def test_charspot_tail_parts_as_the_rust_port():
    # the cases of bunko_vlm::textbox's tail_parts tests
    assert T.charspot_tail_parts([3, 10, 0, 5], 1 << 18, 512) == [[1, 3, 0]]
    assert T.charspot_tail_parts([3, 10, 0, 5], 250, 512) == [[1, 3], [0]]
    assert T.charspot_tail_parts([0, 0]) == []
    assert T.charspot_tail_parts([3, 600]) == [[1], [0]]
    assert T.charspot_tail_parts([362, 362]) == [[0, 1]]
    assert T.charspot_tail_parts([363, 363]) == [[0], [1]]
    assert [len(p) for p in T.charspot_tail_parts([1] * 513)] == [512, 1]


def test_charspot_result_is_unrounded():
    r = T.charspot_result([1, 0], [2, 0], [[0, 0, 8, 8], [8.25, 0, 16, 8]], [0.9, 0.123456], (0.5, 2.0), ["x", "y", "z"])
    assert r == {"text": "xz", "chars": [["x", 4.125, 0.0, 8.0, 16.0, 0.123456], ["z", 0.0, 0.0, 4.0, 16.0, 0.9]]}
