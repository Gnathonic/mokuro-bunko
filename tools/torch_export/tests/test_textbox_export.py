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
