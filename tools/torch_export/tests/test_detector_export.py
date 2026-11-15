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
    assert verify.crop_edges([0.99, 2.0, 10.01, 5.0], (640, 480)) == (1, 2, 10, 5)


def test_crop_edges_round_to_the_nearest_pixel_as_page_rs():
    import numpy as np

    page = (100, 100)
    edge = lambda y: verify.crop_edges([0.0, 0.0, 20.0, y], page)[3]  # noqa: E731
    # a hair either side of a whole pixel is that pixel (f32 values, as the blocks are)
    assert edge(float(np.float32(53.000015))) == edge(53.0) == edge(float(np.float32(52.999985))) == 53
    assert edge(52.6) == 53 and edge(52.4) == 52
    # half away from zero, not Python's half to even
    assert edge(52.5) == 53 and edge(51.5) == 52
    assert verify._round_edge(-2.5) == -3 and verify._round_edge(0.49999999999999994) == 0
    # a box on the page keeps a pixel where rounding would collapse it
    assert verify.crop_edges([10.6, 20.0, 10.9, 20.3], page) == (11, 20, 12, 21)
    assert verify.crop_edges([99.6, 0.0, 99.9, 1.0], page) == (99, 0, 100, 1)
    assert verify.crop_edges([-3.0, 0.0, 0.2, 1.0], page) == (0, 0, 1, 1)
    # clamped to the page
    assert verify.crop_edges([-4.6, -0.4, 100.7, 100.2], page) == (0, 0, 100, 100)
    # no extent on the page: no crop
    assert verify.crop_edges([10.0, 0.0, 10.0, 5.0], page) is None
    assert verify.crop_edges([100.0, 0.0, 120.0, 5.0], page) is None
    assert verify.crop_edges([0.0, 0.0, 5.0, float("nan")], page) is None


def test_compare_blocks_counts_moved_crop_edges():
    page = (640, 480)
    a = [10.2, 20.5, 30.7, 53.000015]
    same, worst, moved = verify.compare_blocks([(a, 0.9)], [([10.2, 20.5, 30.7, 53.0], 0.9)], page)
    assert same and moved == 0 and worst < 2e-5
    same, _, moved = verify.compare_blocks([(a, 0.9)], [([10.2, 20.496, 30.7, 53.0], 0.9)], page)
    assert same and moved == 1


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
