"""The AnimeText detector exporter's pure parts (CPU only, no model files).

Run from tools/torch_export, with the requirements-detector.txt install on the path:

    PYTHONPATH=<pylib-ULTRALYTICS_VERSION> python -m pytest tests/test_detector_export.py -q
"""

import pytest

onnx = pytest.importorskip("onnx")
from torch_export import detector as D, verify  # noqa: E402


def test_gammas_alias_their_folded_initializers():
    inits = {"model.0.conv.weight": ((8, 3, 3, 3), 1, True), "onnx::Mul_2760": ((1, 768, 1, 1), 1, True)}
    m = D.onnx_weight_map(["m.model.0.conv.weight", "m.model.6.gamma"], "m.", inits)
    assert m == {"m.model.0.conv.weight": "model.0.conv.weight", "m.model.6.gamma": "onnx::Mul_2760"}


def test_refuses_a_non_raw_initializer():
    with pytest.raises(SystemExit, match="raw"):
        D.onnx_weight_map(["m.a"], "m.", {"a": ((1,), 1, False)})
    with pytest.raises(SystemExit, match="FLOAT"):
        D.onnx_weight_map(["m.a"], "m.", {"a": ((1,), 10, True)})


def test_meta_and_edges():
    meta = D.detector_meta(D.ANIMETEXT_PIN)
    assert meta["io"] == "detect-1"
    assert meta["weights_format"] == "onnx"
    assert meta["weights_size"] == "236662175"
    assert meta["ultralytics"] == D.ULTRALYTICS_VERSION
    assert D.ULTRALYTICS_VERSION in ("8.3.105", "8.4.212")
    assert verify.crop_edges([0.99, 2.0, 10.01, 5.0]) == (0, 2, 11, 5)


def test_the_requirements_pin_the_same_ultralytics():
    from pathlib import Path

    req = (Path(__file__).parent.parent / "requirements-detector.txt").read_text()
    assert f"ultralytics=={D.ULTRALYTICS_VERSION}" in req


def test_detector_manifest_with_the_real_pin(tmp_path):
    import zipfile

    from torch_export import manifest

    d = tmp_path / "animetext-blocks" / "fp32" / "linux-rocm-gfx1201"; d.mkdir(parents=True)
    with zipfile.ZipFile(d / "detector.pt2", "w") as z:  # a package: one top folder "detector/"
        z.writestr("detector/data/aotinductor/model/x.wrapper.so", b"pk")
    (d / "build.json").write_text('{"graphs": {"detector": {"file": "detector.pt2"}}}')
    m = manifest.build_textbox_manifest(tmp_path, "detector-torch-v1", ("animetext-blocks",),
                                        {"animetext-blocks": D.ANIMETEXT_PIN}, "AGPL-3.0")
    [row] = m["files"]
    assert m["release"] == "detector-torch-v1"
    assert row["id"] == "torch/animetext-blocks/fp32/linux-rocm-gfx1201/detector.pt2"
    assert row["licence"] == "AGPL-3.0"
    assert row["sources"] == [{"repo": "deepghs/AnimeText_yolo", "revision": "a180c191bfdb9f0e31b57e7de567e7b6bac50f84"}]


def test_a_detector_package_with_weights_inside_is_refused(tmp_path):
    import zipfile

    d = tmp_path / "animetext-blocks" / "fp32" / "linux-rocm-gfx1201"; d.mkdir(parents=True)
    with zipfile.ZipFile(d / "detector.pt2", "w") as z:  # a quarter of the ONNX file plus one byte
        z.writestr("m/data/aotinductor/model/x.wrapper.so", b"\0" * (D.ANIMETEXT_PIN.size // 4 + 1))
    assert verify.weights_inside(d / "detector.pt2", D.ANIMETEXT_PIN.size)
    with pytest.raises(SystemExit, match="detector.pt2"):
        verify.refuse_embedded(tmp_path, lambda engine: D.ANIMETEXT_PIN.size)
