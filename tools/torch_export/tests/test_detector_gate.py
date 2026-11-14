"""The AnimeText detector's build checks, gate bundles and release stage (CPU only, fake
packages; no model files).

Run from tools/torch_export:

    python -m pytest tests/test_detector_gate.py -q
"""

import hashlib
import json
import os
import subprocess
import sys
import zipfile
from pathlib import Path

import pytest

from torch_export import detector as D, detector_gate as G, targets  # noqa: E402

AGPL = "GNU AFFERO GENERAL PUBLIC LICENSE\nVersion 3, 19 November 2007\n(the text)\n"


def small_pin(tmp_path):
    """A pin over a small stand-in for the ONNX file."""
    onnx = tmp_path / "animetext_yolo12x.onnx"
    onnx.write_bytes(b"onnx stand-in" * 10000)  # big enough that a fake package holds no weights
    return onnx, D.OnnxPin(repo=D.ANIMETEXT_PIN.repo, revision=D.ANIMETEXT_PIN.revision, file=onnx.name,
                           sha256=hashlib.sha256(onnx.read_bytes()).hexdigest(), size=onnx.stat().st_size)


def fake_build(out, target, pin, commit="a" * 40, dirty=False, meta=None, record=None, unbound=0, n=3):
    """A build record and a fake detector package (zip, one top folder "detector/")."""
    tdir = out / G.ENGINE / G.PRECISION / target
    tdir.mkdir(parents=True)
    fqns = [f"net.model.{i}.conv.weight" for i in range(n)]
    wm = {f: f.removeprefix("net.") for f in fqns[: n - unbound]}
    m = {"bunko.engine": G.ENGINE, "bunko.role": "detector", "bunko.precision": "fp32", "bunko.target": target,
         "bunko.io": D.IO, "bunko.torch": "2.13.0+rocm7.1", "bunko.weights": json.dumps(wm),
         "bunko.weights_format": "onnx", "bunko.weights_sha256": pin.sha256, "bunko.weights_size": str(pin.size),
         "bunko.ultralytics": D.ULTRALYTICS_VERSION, "bunko.max_batch": "8",
         "bunko.exporter_commit": commit + ("-dirty" if dirty else ""), **(meta or {})}
    cpp = "\n".join(f'constants_info_[{i}].original_fqn = "{f}";' for i, f in enumerate(fqns))
    p = tdir / "detector.pt2"
    with zipfile.ZipFile(p, "w") as z:
        z.writestr("detector/data/aotinductor/model/x.wrapper.cpp", cpp)
        z.writestr("detector/data/aotinductor/model/x.wrapper_metadata.json", json.dumps(m))
        z.writestr("detector/data/aotinductor/model/x.wrapper.so", b"pk")
    rec = {"engine": G.ENGINE, "precision": "fp32", "target": target, "io": D.IO, "torch": "2.13.0+rocm7.1",
           "exporter_commit": commit, "exporter_dirty": dirty, "ultralytics": D.ULTRALYTICS_VERSION,
           "graphs": {"detector": {"file": "detector.pt2", "size": p.stat().st_size,
                                   "sha256": hashlib.sha256(p.read_bytes()).hexdigest(), "constants": n}},
           **(record or {})}
    (tdir / "build.json").write_text(json.dumps(rec))
    return tdir


def git_repo(tmp_path):
    """A repository holding stand-ins for the exporter's sources; returns (repo, commit)."""
    repo = tmp_path / "repo"
    for f in ("tools/torch_export/torch_export/__init__.py", "tools/onnx_export/onnx_export/__init__.py"):
        (repo / f).parent.mkdir(parents=True, exist_ok=True)
        (repo / f).write_text("# stand-in\n")
    (repo / "README").write_text("not a source\n")
    env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
           "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com"}
    for cmd in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "sources"]):
        subprocess.run(["git", "-C", str(repo), *cmd], check=True, env=env, capture_output=True)
    sha = subprocess.run(["git", "-C", str(repo), "rev-parse", "HEAD"], check=True, capture_output=True,
                         text=True).stdout.strip()
    return repo, sha


def pages(d, n, suffix=".png"):
    from PIL import Image

    d.mkdir(parents=True, exist_ok=True)
    out = []
    for i in range(n):
        p = d / f"p{i:03d}{suffix}"
        Image.new("RGB", (40 + i, 60), (i, 255 - i, 128)).save(p)
        out.append(p)
    return out


def test_the_detector_targets_are_decision_l8s_ten():
    assert len(G.TARGETS) == len(set(G.TARGETS)) == 10
    for t in G.TARGETS:
        assert "fp32" in targets.get(t).precisions, t
    assert set(G.NEIGHBOURS) < set(G.TARGETS)
    for t, ns in G.NEIGHBOURS.items():
        assert all(n in G.TARGETS and n not in G.NEIGHBOURS for n in ns), t


def test_ship_targets_follow_the_gates():
    cards = ["linux-rocm-gfx1201", "linux-rocm-gfx1030", "linux-cuda-sm_80", "windows-cuda-sm_80"]
    assert G.ship_targets(cards, G.TARGETS) == list(G.TARGETS)
    # gfx1030 did not pass: the ROCm targets without a card wait for it; sm_75 for sm_80
    assert G.ship_targets(["linux-rocm-gfx1201"], G.TARGETS) == ["linux-rocm-gfx1201"]
    got = G.ship_targets(cards[:3], G.TARGETS)
    assert "windows-cuda-sm_80" not in got and "windows-cuda-sm_75" not in got
    assert "linux-cuda-sm_75" in got and "linux-rocm-gfx1100" in got
    # a target that was not built is not shipped, and a passed one must have been built
    assert "linux-rocm-gfx1200" not in G.ship_targets(cards, [t for t in G.TARGETS if t != "linux-rocm-gfx1200"])
    with pytest.raises(SystemExit, match="passed but not built"):
        G.ship_targets(cards, ["linux-rocm-gfx1201"])
    with pytest.raises(SystemExit, match="not a detector target"):
        G.ship_targets(["linux-cuda-sm_120"], G.TARGETS)


def test_a_clean_build_passes_the_check(tmp_path):
    _, pin = small_pin(tmp_path)
    fake_build(tmp_path / "out", "linux-rocm-gfx1201", pin)
    b = G.check_build(tmp_path / "out", "linux-rocm-gfx1201", pin)
    assert (b["constants"], b["commit"], b["ultralytics"]) == (3, "a" * 40, D.ULTRALYTICS_VERSION)


@pytest.mark.parametrize("kw, match", [
    ({"dirty": True}, "modified or unknown tree"),
    ({"commit": "unknown"}, "modified or unknown tree"),
    ({"meta": {"bunko.io": "textbox-1"}}, "bunko.io"),
    ({"meta": {"bunko.weights_format": "safetensors"}}, "bunko.weights_format"),
    ({"meta": {"bunko.ultralytics": "8.3.105"}}, "bunko.ultralytics"),
    ({"meta": {"bunko.max_batch": "4"}}, "bunko.max_batch"),
    ({"meta": {"bunko.target": "linux-rocm-gfx1030"}}, "bunko.target"),
    ({"unbound": 1}, "without an ONNX initializer"),
    ({"record": {"target": "linux-rocm-gfx1030"}}, "target"),
    ({"record": {"exporter_commit": "b" * 40}}, "exporter commit"),
])
def test_a_bad_build_is_refused(tmp_path, kw, match):
    _, pin = small_pin(tmp_path)
    fake_build(tmp_path / "out", "linux-rocm-gfx1201", pin, **kw)
    with pytest.raises(SystemExit, match=match):
        G.check_build(tmp_path / "out", "linux-rocm-gfx1201", pin)


def test_a_package_changed_after_its_record_is_refused(tmp_path):
    _, pin = small_pin(tmp_path)
    tdir = fake_build(tmp_path / "out", "linux-rocm-gfx1201", pin)
    with zipfile.ZipFile(tdir / "detector.pt2", "a") as z:
        z.writestr("detector/extra", b"x")
    with pytest.raises(SystemExit, match="sha256 differs"):
        G.check_build(tmp_path / "out", "linux-rocm-gfx1201", pin)


def test_a_constant_count_other_than_the_records_is_refused(tmp_path):
    _, pin = small_pin(tmp_path)
    tdir = fake_build(tmp_path / "out", "linux-rocm-gfx1201", pin)
    rec = json.loads((tdir / "build.json").read_text())
    rec["graphs"]["detector"]["constants"] = 422
    (tdir / "build.json").write_text(json.dumps(rec))
    with pytest.raises(SystemExit, match="3 constants, 422"):
        G.check_build(tmp_path / "out", "linux-rocm-gfx1201", pin)


def test_builds_from_two_commits_are_refused(tmp_path):
    _, pin = small_pin(tmp_path)
    fake_build(tmp_path / "out", "linux-rocm-gfx1201", pin, commit="a" * 40)
    fake_build(tmp_path / "out", "linux-rocm-gfx1030", pin, commit="b" * 40)
    with pytest.raises(SystemExit, match="2 exporter commits"):
        G.check_builds(tmp_path / "out", ["linux-rocm-gfx1201", "linux-rocm-gfx1030"], pin)
    with pytest.raises(SystemExit, match="not a detector target"):
        G.check_build(tmp_path / "out", "linux-cuda-sm_120", pin)


def test_the_page_list_is_written_once_without_repeats(tmp_path):
    a = pages(tmp_path / "vol-a", 3)
    b = pages(tmp_path / "vol-b", 2, ".jpg")
    lst = tmp_path / "more.txt"
    lst.write_text(f"vol-a/{a[0].name}\n{b[1]}\n")  # both repeats
    got = G.write_page_list([tmp_path / "vol-a", tmp_path / "vol-b", lst], tmp_path / "gate" / "pages.txt", minimum=5)
    assert got == [p.resolve() for p in a + b]
    from torch_export.verify import page_files

    assert page_files(tmp_path / "gate" / "pages.txt") == got
    with pytest.raises(SystemExit, match="5 pages; the gate needs at least 300"):
        G.write_page_list([tmp_path / "vol-a", tmp_path / "vol-b"], tmp_path / "x.txt")


def test_odd_pages_are_odd_canvases(tmp_path):
    from PIL import Image

    src = pages(tmp_path / "src", 2)
    made = G.odd_pages(src, tmp_path / "odd")
    sizes = {p.stem: Image.open(p).size for p in made}
    assert sizes["odd-scrap-3x2"] == (3, 2) and sizes["odd-scrap-31x17"] == (31, 17)
    assert sizes["odd-square-640"] == (640, 640) and sizes["odd-odd-641x639"] == (641, 639)
    assert sizes["odd-spread"] == (40 + 41, 60) and sizes["odd-double"] == (82, 120)
    assert sizes["odd-strip"] == (40, 10) and sizes["odd-column"] == (5, 60)
    again = G.odd_pages(src, tmp_path / "odd2")
    assert [p.read_bytes() for p in made] == [p.read_bytes() for p in again]


def test_a_gate_bundle_holds_everything_it_runs_on(tmp_path):
    onnx, pin = small_pin(tmp_path)
    repo, sha = git_repo(tmp_path)
    fake_build(tmp_path / "out", "linux-rocm-gfx1201", pin, commit=sha)
    src = pages(tmp_path / "pages", 3) + pages(tmp_path / "pages2", 1, ".JPG")
    lst = tmp_path / "pages.txt"
    lst.write_text("".join(f"{p}\n" for p in src))
    dest = tmp_path / "bundle" / "animetext-linux-rocm-gfx1201"
    G.stage_bundle(tmp_path / "out", "linux-rocm-gfx1201", lst, onnx, dest, repo=repo, pin=pin)

    info = json.loads((dest / "bundle.json").read_text())
    assert (info["target"], info["commit"], info["pages"]) == ("linux-rocm-gfx1201", sha, 4)
    assert (dest / "pages.txt").read_text().split() == ["pages/0001.png", "pages/0002.png", "pages/0003.png",
                                                        "pages/0004.jpg"]
    assert (dest / "pages/0004.jpg").read_bytes() == src[3].read_bytes()
    assert (dest / info["onnx"]).read_bytes() == onnx.read_bytes()
    assert (dest / "package/animetext-blocks/fp32/linux-rocm-gfx1201/detector.pt2").is_file()
    assert (dest / "src/tools/torch_export/torch_export/__init__.py").is_file()
    assert (dest / "src/tools/onnx_export/onnx_export/__init__.py").is_file()
    assert not (dest / "src/README").exists()
    assert os.access(dest / "run.sh", os.X_OK)
    listed = {line.split("  ", 1)[1] for line in (dest / "SHA256SUMS").read_text().splitlines()}
    assert listed == {p.relative_to(dest).as_posix() for p in dest.rglob("*") if p.is_file()} - {"SHA256SUMS"}
    subprocess.run(["sha256sum", "-c", "--quiet", "SHA256SUMS"], cwd=dest, check=True)
    assert not dest.with_name(dest.name + ".partial").exists()

    with pytest.raises(SystemExit, match="exists"):
        G.stage_bundle(tmp_path / "out", "linux-rocm-gfx1201", lst, onnx, dest, repo=repo, pin=pin)


def test_a_bundle_needs_the_pinned_onnx_file_and_a_known_commit(tmp_path):
    onnx, pin = small_pin(tmp_path)
    repo, _ = git_repo(tmp_path)
    fake_build(tmp_path / "out", "linux-rocm-gfx1201", pin, commit="c" * 40)
    pages(tmp_path / "pages", 2)
    dest = tmp_path / "b"
    with pytest.raises(SystemExit, match="not a commit"):
        G.stage_bundle(tmp_path / "out", "linux-rocm-gfx1201", tmp_path / "pages", onnx, dest, repo=repo, pin=pin)
    onnx.write_bytes(b"another file")
    with pytest.raises(SystemExit, match="bytes, pinned"):
        G.stage_bundle(tmp_path / "out", "linux-rocm-gfx1201", tmp_path / "pages", onnx, dest, repo=repo, pin=pin)
    assert not dest.exists()


def _bundle(tmp_path, pages_n=300):
    b = tmp_path / "bundle"
    b.mkdir()
    (b / "bundle.json").write_text(json.dumps({
        "engine": G.ENGINE, "precision": "fp32", "target": "linux-cuda-sm_80", "commit": "a" * 40,
        "pages": pages_n, "page_list": "pages.txt", "onnx": "onnx/x.onnx", "device": "cuda", "bench_batches": [1, 4, 8]}))
    return b


def test_the_gate_run_writes_its_summary(tmp_path):
    b = _bundle(tmp_path)
    seen = {}

    def verify(bundle, info):
        seen["info"] = info
        return {"pages": 300, "identical_sets": 300, "worst_corner": 0.004, "edges_moved": 0, "blocks": 2500,
                "differ": [], "differ_pages": [], "pages_per_s": {"1": 20.0, "4": 24.0, "8": 25.0},
                "package_ms_per_page": 30.0, "returncode": 0}

    s = G.run_gate(b, verify)
    assert s["passed"] and s["pages_per_s"]["4"] == 24.0
    assert json.loads((b / "results" / "summary.json").read_text()) == s
    assert seen["info"]["target"] == "linux-cuda-sm_80"


@pytest.mark.parametrize("res, error", [
    ({"identical_sets": 299, "differ_pages": ["pages/0007.png"],
      "differ": [{"page": "pages/0007.png", "onnx": [], "package": [[[0, 0, 1, 1], 0.5]]}], "returncode": 1}, None),
    ({"worst_corner": 0.02, "returncode": 1}, None),
    ({"edges_moved": 1, "returncode": 1}, None),
    ({"pages": 299, "identical_sets": 299}, "the bundle has 300"),
    ({"error": "verify exited 3 without a result", "returncode": 3}, "without a result"),
])
def test_a_failed_gate_run_says_why(tmp_path, res, error):
    b = _bundle(tmp_path)
    base = {"pages": 300, "identical_sets": 300, "worst_corner": 0.0, "edges_moved": 0, "blocks": 10,
            "differ": [], "differ_pages": [], "returncode": 0}
    s = G.run_gate(b, lambda bundle, info: {**base, **res})
    assert not s["passed"]
    if error:
        assert error in s["error"]
    if res.get("differ"):
        assert s["first_differ"]["page"] == "pages/0007.png" and s["differ_pages"] == ["pages/0007.png"]


def test_the_agpl_text_comes_from_ultralytics_dist_info(tmp_path):
    lic = tmp_path / f"ultralytics-{D.ULTRALYTICS_VERSION}.dist-info" / "licenses" / "LICENSE"
    lic.parent.mkdir(parents=True)
    lic.write_text(AGPL)
    assert G.agpl_text(search=[str(tmp_path)]) == AGPL
    other = tmp_path / "MIT"
    other.write_text("MIT License\n")
    with pytest.raises(SystemExit, match="not the AGPL-3.0"):
        G.agpl_text(other)


def test_the_release_stage_holds_what_may_ship(tmp_path, monkeypatch):
    _, pin = small_pin(tmp_path)
    here = Path(G.__file__).resolve().parents[1]
    sha = subprocess.run(["git", "-C", str(here), "rev-parse", "HEAD"], check=True, capture_output=True,
                         text=True).stdout.strip()
    out = tmp_path / "out"
    for t in ("linux-rocm-gfx1201", "linux-rocm-gfx1030", "linux-rocm-gfx1100", "linux-cuda-sm_80"):
        fake_build(out, t, pin, commit=sha)
    lic = tmp_path / "LICENSE.agpl"
    lic.write_text(AGPL)
    stage = tmp_path / "stage" / G.RELEASE
    # gfx1030 failed its gate: it and gfx1100 (whose neighbour it is) are left out
    m = G.stage_release(out, stage, ["linux-rocm-gfx1201", "linux-cuda-sm_80"],
                        {"linux-rocm-gfx1201": "an RX 9070 XT"}, lic, pin=pin)
    assert [r["target"] for r in m["files"]] == ["linux-cuda-sm_80", "linux-rocm-gfx1201"]
    assert all(r["licence"] == "AGPL-3.0" and not r.get("requires") for r in m["files"])
    assert sorted(p.name for p in stage.iterdir()) == sorted(
        ["LICENSE", "NOTICE.md", f"{G.RELEASE}.json", *(r["file"] for r in m["files"])])
    assert not stage.with_name(stage.name + ".select").exists()
    text = (stage / "NOTICE.md").read_text()
    for want in (sha, f"ultralytics {D.ULTRALYTICS_VERSION}", "detector.py", "verified on an RX 9070 XT",
                 "| `linux-cuda-sm_80` | `animetext-blocks_fp32_linux-cuda-sm_80_detector.pt2` | verified on a test card |",
                 pin.sha256, "A2C2f"):
        assert want in text, want
    assert "gfx1100" not in text and "gfx1030" not in text
    assert (stage / "LICENSE").read_text() == AGPL
    with pytest.raises(SystemExit, match="not empty"):
        G.stage_release(out, stage, ["linux-rocm-gfx1201"], None, lic, pin=pin)

    # publish_detector.sh takes the stage through its own checks, up to the commit's
    # publication (which this test does not reach: it never fetches)
    r = subprocess.run(["bash", str(here / "publish_detector.sh"), str(stage)], capture_output=True, text=True,
                       env={**os.environ, "PYTHON": sys.executable, "PUBLISH_NO_FETCH": "1"})
    assert "2 packages match detector-torch-v1.json" in r.stderr, r.stderr


def test_a_dirty_build_is_never_staged(tmp_path):
    _, pin = small_pin(tmp_path)
    fake_build(tmp_path / "out", "linux-rocm-gfx1201", pin, dirty=True)
    lic = tmp_path / "LICENSE.agpl"
    lic.write_text(AGPL)
    with pytest.raises(SystemExit, match="modified or unknown tree"):
        G.stage_release(tmp_path / "out", tmp_path / "stage", ["linux-rocm-gfx1201"], None, lic, pin=pin)
    assert not (tmp_path / "stage").exists() or not any((tmp_path / "stage").iterdir())


def test_the_bench_times_each_batch_on_one_canvas():
    import numpy as np
    import torch

    from torch_export import verify

    calls = []

    def m(x):
        calls.append(tuple(x.shape))
        return (torch.zeros(x.shape[0], 5, 10),)

    x = np.zeros((3, 64, 96), np.float32)
    r = verify.detector_bench(m, torch.device("cpu"), x, (1, 4, 8), runs=3, warmup=1)
    assert list(r) == ["1", "4", "8"] and all(v > 0 for v in r.values())
    assert calls.count((4, 3, 64, 96)) == 4  # the warm-up run, then the timed ones
    with pytest.raises(SystemExit, match="bunko.max_batch"):
        verify.detector_bench(m, torch.device("cpu"), x, (16,))
    assert verify.batches_arg("1,4,8") == (1, 4, 8)
    with pytest.raises(ValueError):
        verify.batches_arg("0,4")


def test_the_cli_checks_builds_and_stages(tmp_path, capsys):
    from torch_export.__main__ import main

    onnx, pin = small_pin(tmp_path)
    out = tmp_path / "out"
    fake_build(out, "linux-rocm-gfx1201", pin)
    # the real pin: a stand-in build carries the small pin's sha256, so the check refuses it
    with pytest.raises(SystemExit, match="bunko.weights_sha256"):
        main(["detector-check", "--out", str(out), "-t", "linux-rocm-gfx1201"])
    with pytest.raises(SystemExit, match="no build record"):
        main(["detector-check", "--out", str(out)])
    src = pages(tmp_path / "vol", 4)
    lst = tmp_path / "gate" / "pages.txt"
    assert main(["detector-pages", "--list", str(lst), "--odd", str(tmp_path / "odd"), "--min", "12",
                 str(tmp_path / "vol")]) == 0
    from torch_export.verify import page_files

    got = page_files(lst)
    assert got[:4] == [p.resolve() for p in src] and len(got) == 12
    with pytest.raises(SystemExit, match="TARGET=CARD"):
        main(["detector-stage", "--out", str(out), "--stage", str(tmp_path / "s"), "--passed", "linux-rocm-gfx1201",
              "--verified", "linux-rocm-gfx1201"])
