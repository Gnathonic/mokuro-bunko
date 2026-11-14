"""``torch-models.json``: every built package and weights file, in the models.json format.

Same envelope and per-file fields as tools/onnx_export's models.json (MODELS.md §4) --
engine, file (flat release asset name), url, size, sha256, licence, sources,
export_tool_version, kind, role, precision -- plus, for the libtorch backend:

  id, path  store path under <storage>/models/ (torch/<engine>/<precision>/[<target>/]<name>);
            the id IS the path (unique across the release)
  unpack_to aoti only: the package directory the runtime loads in place. The .pt2 is a
            stored zip with exactly one top folder "<role>/" (checked here): the installer
            extracts that folder's contents into unpack_to (strip the top component).
  kind      "aoti" (a .pt2 package) | "aoti-weights" (a weights-<group>.safetensors file)
  target    aoti only: linux-cuda-sm_86, linux-rocm-gfx1030, linux-cpu-x86_64-v3, ...
  torch     the exact torch the package was compiled with (the backend pack must match)
  io        graph I/O contract version (TORCH-BACKEND.md)
  requires  aoti only: ids of the weights files it binds at load (every release package);
            empty only for packages built with their weights inside
            (TORCH_EXPORT_CPU_EMBED=1 / TORCH_EXPORT_WIN_EMBED=1)

Build records (``build.json``) are the source of truth; files present on disk but not in a
build record are refused, as are records whose sha256 no longer matches the file.

Packages that bind their authors' own weights (text-box recognizers, AnimeText) go into
releases of their own (``textbox-torch-v1``, ``detector-torch-v1``) through
``build_textbox_manifest``: same schema, package rows only, no ``requires``, the source
and licence passed in.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Mapping

from . import IO_VERSION, TOOL_VERSION
from .build import sha256

FORMAT = "mokuro-bunko-models/1"
URL = "https://github.com/Gnathonic/mokuro-bunko/releases/download/{tag}/{file}"


def _sources(engine: str) -> list[dict]:
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "onnx_export"))
    from onnx_export.pins import ENGINE_SOURCES

    return [{"repo": s.repo, "revision": s.revision, "licence": s.licence} for s in ENGINE_SOURCES[engine]]


def build_manifest(out: Path, tag: str, base_url: str | None = None, flat_dir: Path | None = None) -> dict:
    files: list[dict] = []
    seen: set[Path] = set()
    url = (base_url.rstrip("/") + "/{file}") if base_url else URL.replace("{tag}", tag)
    for rec_path in sorted(out.glob("*/*/*/build.json")):
        rec = json.loads(rec_path.read_text())
        eng, prec, target = rec["engine"], rec["precision"], rec["target"]
        tdir = rec_path.parent
        if "authors_weights" in rec:
            # a package bound to its authors' weights (nar-small, ...): a release of its
            # own, through textbox-manifest; its files are accounted for, not stray
            seen.update(tdir / g["file"] for g in rec.get("graphs", {}).values())
            print(f"{rec_path}: {eng} binds its authors' weights; left out (use textbox-manifest)",
                  file=sys.stderr)
            continue
        srcs = _sources(eng)
        lic = " AND ".join(sorted({s["licence"] for s in srcs}))
        weights = sorted(rec.get("weights", {}))
        for wname in weights:
            p = tdir.parent / wname
            if p in seen:
                continue
            seen.add(p)
            info = rec["weights"][wname]
            digest = sha256(p)
            if digest != info["sha256"]:
                raise SystemExit(f"{p}: sha256 differs from {rec_path}")
            flat = f"{eng}_{prec}_{wname}"
            files.append({"engine": eng, "file": flat, "url": url.format(file=flat), "size": p.stat().st_size,
                          "sha256": digest, "licence": lic, "sources": srcs, "export_tool_version": rec["tool"],
                          "kind": "aoti-weights", "role": "weights-" + wname.split("-", 1)[1].split(".")[0],
                          "precision": prec, "id": f"torch/{eng}/{prec}/{wname}", "path": f"torch/{eng}/{prec}/{wname}",
                          # shared by every target (cpu, cu130, rocm7.1 builds): no variant
                          "torch": rec["torch"].split("+")[0], "io": rec["io"]})
        for role, g in rec["graphs"].items():
            p = tdir / g["file"]
            seen.add(p)
            digest = sha256(p)
            if digest != g["sha256"]:
                raise SystemExit(f"{p}: sha256 differs from {rec_path} (rebuilt without a new record?)")
            import zipfile

            with zipfile.ZipFile(p) as z:
                tops = {n.split("/", 1)[0] for n in z.namelist()}
            if tops != {role}:
                raise SystemExit(f"{p}: archive top folder {tops} is not {{'{role}'}}")
            flat = f"{eng}_{prec}_{target}_{g['file']}"
            files.append({"engine": eng, "file": flat, "url": url.format(file=flat), "size": p.stat().st_size,
                          "sha256": digest, "licence": lic, "sources": srcs, "export_tool_version": rec["tool"],
                          "kind": "aoti", "role": role, "precision": prec, "target": target,
                          "id": f"torch/{eng}/{prec}/{target}/{g['file']}", "path": f"torch/{eng}/{prec}/{target}/{g['file']}",
                          # the archive has one top folder "<role>/": extract its CONTENTS
                          # (strip that folder) into unpack_to, else <role>/<role>/ results
                          "unpack_to": f"torch/{eng}/{prec}/{target}/{role}/", "torch": rec["torch"], "io": rec["io"],
                          # the weights group this graph binds (vision -> weights-vision,
                          # prefill/step -> weights-decoder); none when the weights are inside
                          "requires": [f"torch/{eng}/{prec}/{w}" for w in weights
                                       if w == f"weights-{'vision' if role == 'vision' else 'decoder'}.safetensors"]
                          if g.get("constants") else []})
    ids = {f["id"] for f in files}
    dangling = sorted({r for f in files for r in f.get("requires", []) if r not in ids})
    if dangling:
        raise SystemExit(f"requires entries without a file in this manifest: {dangling[:5]}")
    stray = [p for p in out.rglob("*") if p.suffix in (".pt2", ".safetensors") and p not in seen]
    if stray:
        raise SystemExit(f"files without a build record: {[str(p) for p in stray[:5]]}")
    m = {"format": FORMAT, "release": tag, "export_tool": "tools/torch_export (mokuro-bunko-torch-export)",
         "export_tool_version": TOOL_VERSION, "io_version": IO_VERSION, "files": files}
    (out / "torch-models.json").write_text(json.dumps(m, indent=1) + "\n")
    if flat_dir is not None:
        # the release assets under their flat names (hard links): a GitHub-release-shaped
        # directory, usable as MOKURO_TORCH_MODELS_MIRROR
        import os

        flat_dir.mkdir(parents=True, exist_ok=True)
        for f in files:
            dst = flat_dir / f["file"]
            dst.unlink(missing_ok=True)
            os.link(out / f["path"].removeprefix("torch/"), dst)
        (flat_dir / "torch-models.json").write_text(json.dumps(m, indent=1) + "\n")
    big = max((f["size"] for f in files), default=0)
    print(f"wrote {out / 'torch-models.json'}: {len(files)} files, {sum(f['size'] for f in files):,} bytes, "
          f"largest {big:,}", file=sys.stderr)
    return m


def build_textbox_manifest(out: Path, tag: str, engines: tuple[str, ...], pins: Mapping[str, object], licence: str,
                           base_url: str | None = None, flat_dir: Path | None = None) -> dict:
    """The release of packages that bind their authors' own weights (``textbox-torch-v1``:
    nar-small, charspot-small; ``detector-torch-v1``: animetext-blocks), in the
    ``torch-models.json`` schema with its own ``release``: one row per graph and no
    weights rows, so no row carries ``requires`` (a ``requires`` entry must name a file
    of the same release; the weights are host files of the base manifest).

    ``pins[engine]`` is any object with ``repo`` and ``revision`` (the weights' source);
    when it also has ``size`` (``textbox.AuthorsWeights``), a package holding an entry
    bigger than ``size // verify.EMBED_FRACTION`` is refused. Every build record under
    ``out/<engine>/<precision>/<target>/build.json`` of the named engines is included:
    leave a target out of ``out`` to leave it out of the release. Writes
    ``out/<tag>.json`` (and, with ``flat_dir``, the assets under their flat names plus
    that file, usable as ``MOKURO_TORCH_MODELS_MIRROR``)."""
    import zipfile

    from . import graphs as G
    from .verify import weights_inside

    out = Path(out)
    url = (base_url.rstrip("/") + "/{file}") if base_url else URL.replace("{tag}", tag)
    files: list[dict] = []
    seen: set[Path] = set()
    for eng in engines:
        pin = pins.get(eng)
        if pin is None:
            raise SystemExit(f"{eng}: no pin for its weights' source; the release cannot name where they come from")
        roles = G.ROLES.get(eng)
        if roles is None:
            raise SystemExit(f"{eng}: no graph roles (graphs.ROLES)")
        recs = sorted((out / eng).glob("*/*/build.json"))
        if not recs:
            raise SystemExit(f"{eng}: no build records under {out / eng}")
        for rec_path in recs:
            rec = json.loads(rec_path.read_text())
            tdir = rec_path.parent
            prec, target = tdir.parent.name, tdir.name
            for field, want in (("engine", eng), ("precision", prec), ("target", target)):
                if rec.get(field, want) != want:
                    raise SystemExit(f"{rec_path}: {field} {rec[field]!r} where the layout says {want!r}")
            graphs = rec.get("graphs", {})
            if set(graphs) != set(roles):
                raise SystemExit(f"{rec_path}: graphs {sorted(graphs)}, {eng} has {list(roles)}")
            for role in roles:
                g = graphs[role]
                if g["file"] != f"{role}.pt2":
                    raise SystemExit(f"{rec_path}: graph {role} is {g['file']}, not {role}.pt2")
                p = tdir / g["file"]
                seen.add(p)
                digest = sha256(p)
                if g.get("sha256", digest) != digest:
                    raise SystemExit(f"{p}: sha256 differs from {rec_path} (rebuilt without a new record?)")
                try:
                    with zipfile.ZipFile(p) as z:
                        tops = {n.split("/", 1)[0] for n in z.namelist()}
                except zipfile.BadZipFile:
                    raise SystemExit(f"{p}: not a package (no zip)") from None
                if tops != {role}:
                    raise SystemExit(f"{p}: archive top folder {tops} is not {{'{role}'}}")
                size = getattr(pin, "size", None)
                if size and weights_inside(p, size):
                    raise SystemExit(f"{p}: weights inside the package")
                flat = f"{eng}_{prec}_{target}_{g['file']}"
                rel = f"torch/{eng}/{prec}/{target}/{g['file']}"
                row = {"engine": eng, "file": flat, "url": url.format(file=flat), "size": p.stat().st_size,
                       "sha256": digest, "licence": licence,
                       "sources": [{"repo": pin.repo, "revision": pin.revision}],
                       "kind": "aoti", "role": role, "precision": prec, "target": target, "id": rel, "path": rel,
                       # the archive has one top folder "<role>/": the store extracts its
                       # CONTENTS into unpack_to (as for the base release's packages)
                       "unpack_to": f"torch/{eng}/{prec}/{target}/{role}/"}
                for key, field in (("tool", "export_tool_version"), ("torch", "torch"), ("io", "io")):
                    if key in rec:
                        row[field] = rec[key]
                files.append(row)
        stray = [p for p in (out / eng).rglob("*") if p.suffix in (".pt2", ".safetensors") and p not in seen]
        if stray:
            raise SystemExit(f"files without a build record (or weights, which this release never holds): "
                             f"{[str(p) for p in stray[:5]]}")
    m = {"format": FORMAT, "release": tag, "export_tool": "tools/torch_export (mokuro-bunko-torch-export)",
         "export_tool_version": TOOL_VERSION, "files": files}
    text = json.dumps(m, indent=1) + "\n"
    (out / f"{tag}.json").write_text(text)
    if flat_dir is not None:
        import os

        flat_dir = Path(flat_dir)
        flat_dir.mkdir(parents=True, exist_ok=True)
        for f in files:
            dst = flat_dir / f["file"]
            dst.unlink(missing_ok=True)
            os.link(out / f["path"].removeprefix("torch/"), dst)
        (flat_dir / f"{tag}.json").write_text(text)
    print(f"wrote {out / (tag + '.json')}: {len(files)} packages, {sum(f['size'] for f in files):,} bytes",
          file=sys.stderr)
    return m
