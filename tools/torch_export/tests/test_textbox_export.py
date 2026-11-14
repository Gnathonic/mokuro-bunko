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
