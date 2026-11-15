"""charspot-small's gate bundle and its run (CPU only, no model files, no GPU).

Run from tools/torch_export: python -m pytest tests/test_charspot_gate.py -q
"""

import hashlib
import json
import subprocess
import zipfile

import pytest
import torch  # noqa: F401  (the exporter's modules need it importable)

from torch_export import charspot_gate as CG, textbox as T

TARGET = "linux-rocm-gfx1201"


def _package(path, role, meta=None, size=2):
    """A weightless .pt2 as the exporter writes it, with its metadata where AOTInductor
    puts it."""
    with zipfile.ZipFile(path, "w") as z:
        z.writestr(f"{role}/data/aotinductor/model/x.wrapper.so", b"\0" * size)
        if meta is not None:
            z.writestr(f"{role}/data/aotinductor/model/x.wrapper_metadata.json", json.dumps(meta))
    return path


def _meta(role, n):
    return {"bunko.engine": "charspot-small", "bunko.role": role, "bunko.precision": "fp32",
            "bunko.target": TARGET, "bunko.io": T.IO, "bunko.weights_sha256": T.CHARSPOT_PIN.sha256,
            "bunko.weights": json.dumps({f"{role}.{role}.k{i}": f"{role}.k{i}" for i in range(n)})}


def _sha(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()


def _repo(tmp_path):
    """A git repository with the two source trees a bundle carries; returns (repo, commit)."""
    repo = tmp_path / "repo"
    for rel in ("tools/torch_export/torch_export/__init__.py", "tools/textbox_export/charspot_reference.py",
                "tools/onnx_export/x.py", "crates/elsewhere.rs"):
        (repo / rel).parent.mkdir(parents=True, exist_ok=True)
        (repo / rel).write_text("# x\n")
    git = ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@example.com"]
    subprocess.run(["git", "init", "-q", str(repo)], check=True)
    subprocess.run([*git, "add", "."], check=True)
    subprocess.run([*git, "commit", "-q", "-m", "x"], check=True)
    sha = subprocess.run([*git, "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
    return repo, sha


def _build(tmp_path, commit=None, dense_constants=3, meta=None, inside=False):
    out = tmp_path / "build"
    d = out / "charspot-small" / "fp32" / TARGET
    d.mkdir(parents=True)
    graphs = {}
    for role, n in (("dense", 3), ("tail", 2)):
        size = T.CHARSPOT_PIN.size // 4 + 1 if inside and role == "dense" else 2
        p = _package(d / f"{role}.pt2", role, (meta or _meta)(role, n), size)
        graphs[role] = {"file": p.name, "sha256": _sha(p), "constants": dense_constants if role == "dense" else n}
    rec = {"engine": "charspot-small", "precision": "fp32", "target": TARGET, "graphs": graphs}
    if commit is not None:
        rec["exporter_commit"] = commit
    (d / "build.json").write_text(json.dumps(rec))
    return out


def _crops(tmp_path, names=("a.png", "b.png")):
    crops = tmp_path / "crops"
    crops.mkdir()
    for n in names:
        (crops / n).write_bytes(b"png:" + n.encode())
    (crops / "notes.txt").write_text("not a crop")
    return crops


def _expected(tmp_path, names=("a.png", "b.png")):
    p = tmp_path / "charspot-fp32-expected.jsonl"
    p.write_text("".join(json.dumps({"file": n, "text": "あ", "chars": [["あ", 0, 0, 8, 8, .9]]}) + "\n"
                         for n in names), encoding="utf-8")
    return p


def _stage(tmp_path, **kw):
    repo, sha = _repo(tmp_path)
    out = _build(tmp_path, commit=kw.pop("commit", sha), **kw)
    return CG.stage_bundle(out, TARGET, _crops(tmp_path), _expected(tmp_path), sha, tmp_path / "bundle",
                           repo=repo), sha


def test_a_bundle_holds_the_packages_crops_expected_and_sources_and_no_weights(tmp_path):
    b, sha = _stage(tmp_path)
    pdir = b / "package" / "charspot-small" / "fp32" / TARGET
    assert sorted(p.name for p in pdir.iterdir()) == ["build.json", "dense.pt2", "tail.pt2"]
    assert sorted(p.name for p in (b / "crops").iterdir()) == ["a.png", "b.png"]
    assert (b / "expected" / CG.EXPECTED).is_file()
    assert (b / "src" / "tools" / "torch_export" / "torch_export" / "__init__.py").is_file()
    assert (b / "src" / "tools" / "textbox_export" / "charspot_reference.py").is_file()
    assert not (b / "src" / "crates").exists()
    assert (b / "run.sh").stat().st_mode & 0o111
    # never the authors' weights, nor anything that could hold them
    assert not [p for p in b.rglob("*") if p.suffix in (".safetensors", ".onnx", ".bin")]
    info = json.loads((b / "bundle.json").read_text())
    assert (info["engine"], info["precision"], info["target"], info["commit"]) == ("charspot-small", "fp32", TARGET, sha)
    assert (info["crops"], info["batches"]) == (2, list(CG.BATCHES))
    assert info["packages"]["dense"]["sha256"] == _sha(pdir / "dense.pt2")
    assert info["weights"] == {"repo": T.CHARSPOT_PIN.repo, "revision": T.CHARSPOT_PIN.revision,
                               "file": T.CHARSPOT_PIN.file, "sha256": T.CHARSPOT_PIN.sha256,
                               "size": T.CHARSPOT_PIN.size}
    # SHA256SUMS names every other file, and they check
    sums = dict(reversed(line.split("  ", 1)) for line in (b / "SHA256SUMS").read_text().splitlines())
    files = sorted(p.relative_to(b).as_posix() for p in b.rglob("*") if p.is_file() and p.name != "SHA256SUMS")
    assert sorted(sums) == files
    assert all(_sha(b / f) == h for f, h in sums.items())
    assert not (tmp_path / "bundle.partial").exists()


def test_a_bundle_is_staged_once(tmp_path):
    (tmp_path / "bundle").mkdir()
    with pytest.raises(SystemExit, match="exists"):
        _stage(tmp_path)


def test_a_package_with_its_weights_inside_is_never_bundled(tmp_path):
    with pytest.raises(SystemExit, match="dense.pt2: weights inside"):
        _stage(tmp_path, inside=True)
    assert not (tmp_path / "bundle").exists() and not (tmp_path / "bundle.partial").exists()


def test_an_unbound_constant_is_refused(tmp_path):
    with pytest.raises(SystemExit, match="dense.pt2: 4 constants, 3 in bunko.weights"):
        _stage(tmp_path, dense_constants=4)


def test_a_package_for_other_weights_is_refused(tmp_path):
    def other(role, n):
        return {**_meta(role, n), "bunko.weights_sha256": "0" * 64}

    with pytest.raises(SystemExit, match="bunko.weights_sha256"):
        _stage(tmp_path, meta=other)


def test_a_package_of_another_io_is_refused(tmp_path):
    def other(role, n):
        return {**_meta(role, n), "bunko.io": "textbox-0"}

    with pytest.raises(SystemExit, match="bunko.io"):
        _stage(tmp_path, meta=other)


def test_a_package_built_from_another_commit_is_refused(tmp_path):
    with pytest.raises(SystemExit, match="built from"):
        _stage(tmp_path, commit="f" * 40)


def test_a_build_with_uncommitted_exporter_changes_is_refused(tmp_path):
    repo, sha = _repo(tmp_path)
    out = _build(tmp_path, commit=sha)
    rec_path = out / "charspot-small" / "fp32" / TARGET / "build.json"
    rec_path.write_text(json.dumps({**json.loads(rec_path.read_text()), "exporter_dirty": True}))
    with pytest.raises(SystemExit, match="uncommitted"):
        CG.stage_bundle(out, TARGET, _crops(tmp_path), _expected(tmp_path), sha, tmp_path / "bundle", repo=repo)


def test_every_crop_needs_an_expected_row(tmp_path):
    repo, sha = _repo(tmp_path)
    out = _build(tmp_path, commit=sha)
    with pytest.raises(SystemExit, match="no expected characters for 1 crops, first \\['b.png'\\]"):
        CG.stage_bundle(out, TARGET, _crops(tmp_path), _expected(tmp_path, ("a.png",)), sha, tmp_path / "bundle",
                        repo=repo)


def _fake_verify(results):
    calls = []

    def verify(bundle, info, batch, expected):
        calls.append(batch)
        return results(batch)

    return verify, calls


def _ok(batch, crops=2):
    return {"crops": crops, "identical": crops, "differ": [], "batch": batch, "passed": True,
            "crops_per_s": 10.0 * batch, "largest_move_px": 1e-3, "returncode": 0}


def test_the_gate_passes_when_every_batch_reads_every_crop_the_same(tmp_path):
    b, _ = _stage(tmp_path)
    verify, calls = _fake_verify(_ok)
    s = CG.run_gate(b, verify)
    assert calls == list(CG.BATCHES)
    assert s["passed"] is True
    assert s["crops_per_s"] == {str(x): 10.0 * x for x in CG.BATCHES}
    assert json.loads((b / "results" / "summary.json").read_text())["passed"] is True


def test_one_batch_that_moves_a_box_fails_the_gate(tmp_path):
    b, _ = _stage(tmp_path)

    def results(batch):
        if batch == 32:
            return {**_ok(batch), "identical": 1, "differ": ["b.png"], "passed": False, "returncode": 1}
        return _ok(batch)

    s = CG.run_gate(b, _fake_verify(results)[0])
    assert s["passed"] is False
    assert s["batches"]["32"]["differ"] == ["b.png"]
    assert s["batches"]["8"]["passed"] is True


def test_a_verify_without_a_result_fails_the_gate(tmp_path):
    b, _ = _stage(tmp_path)
    s = CG.run_gate(b, _fake_verify(lambda batch: {"error": "verify exited 1 without a result", "passed": False})[0])
    assert s["passed"] is False


def test_a_result_over_fewer_crops_than_the_bundle_fails(tmp_path):
    b, _ = _stage(tmp_path)
    s = CG.run_gate(b, _fake_verify(lambda batch: _ok(batch, crops=1))[0])
    assert s["passed"] is False


def test_the_notice_names_the_code_the_commit_and_the_weights(tmp_path):
    rows = [{"file": f"charspot-small_fp32_{TARGET}_{r}.pt2", "size": 3, "sha256": "ab" * 32} for r in ("dense", "tail")]
    text = CG.notice("c" * 40, rows)
    for s in (T.CHARSPOT_PIN.repo, T.CHARSPOT_PIN.revision, T.CHARSPOT_PIN.sha256, "c" * 40, "Apache License 2.0",
              "CC BY-NC-SA 4.0", "manga_ocr_charspot_small/model.py", rows[0]["file"], rows[1]["file"],
              "tools/torch_export/torch_export/textbox.py"):
        assert s in text, s
    assert "no weights" in text.lower()
