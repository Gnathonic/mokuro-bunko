#!/usr/bin/env bash
# Check a staged text-box (or detector) release and print the command that publishes it.
#
#   tools/torch_export/publish_textbox.sh <stage-dir>                  check, then print the command
#   tools/torch_export/publish_textbox.sh --verify-upload <stage-dir>  after the upload: download
#                                                                      every asset, compare sha256
#
# <stage-dir> is what `python -m torch_export textbox-manifest --flat <stage-dir>` writes (the
# packages under their flat release names and <tag>.json), plus NOTICE.md and the authors'
# LICENSE. Before anything else it runs `python -m torch_export refuse-embedded <stage-dir>`
# and stops on a non-zero exit: no package with its weights inside is ever staged or
# uploaded. It also refuses any weights file (the authors' weights are fetched from the
# authors' repository, never from ours), a file the manifest does not name, and a package
# whose size or sha256 differs from its row.
#
# Nothing is uploaded by this script: the owner runs the printed `gh release create
# --prerelease` once (after plans 01-03 pass their gates), then `--verify-upload`.
# Environment: PYTHON (default: python3, with torch_export's requirements), GH_REPO
# (default: Gnathonic/mokuro-bunko).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
py="${PYTHON:-python3}"
repo="${GH_REPO:-Gnathonic/mokuro-bunko}"

mode=check
if [[ "${1:-}" == "--verify-upload" ]]; then
  mode=upload
  shift
fi
if [[ $# -ne 1 || ! -d "$1" ]]; then
  echo "usage: $0 [--verify-upload] <stage-dir>" >&2
  exit 2
fi
stage="$(cd "$1" && pwd)"

# 1. no package with its weights inside, before anything else
(cd "$here" && "$py" -m torch_export refuse-embedded "$stage") || {
  echo "refused: a package in $stage has its weights inside; nothing is staged or uploaded" >&2
  exit 1
}

# 2. the stage holds exactly the manifest's packages, its json, NOTICE.md and LICENSE
"$py" - "$stage" "$mode" "$repo" "$0" <<'EOF'
import hashlib, json, shlex, subprocess, sys, tempfile
from pathlib import Path

stage, mode, repo, me = Path(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]

def sha256(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        while b := f.read(1 << 24):
            h.update(b)
    return h.hexdigest()

jsons = sorted(stage.glob("*.json"))
if len(jsons) != 1:
    sys.exit(f"{stage}: expected one release json (<tag>.json), found {[p.name for p in jsons]}")
m = json.loads(jsons[0].read_text())
tag = m["release"]
if jsons[0].name != f"{tag}.json":
    sys.exit(f"{jsons[0]}: names release {tag!r}")
rows = m["files"]
if not rows:
    sys.exit(f"{jsons[0]}: no files")
for r in rows:
    if r.get("requires"):
        sys.exit(f"{r['file']}: has requires; this release holds packages only")
    if not r["file"].endswith(".pt2"):
        sys.exit(f"{r['file']}: not a package; this release never holds weights")
    if not r["url"].endswith(f"/{tag}/{r['file']}"):
        sys.exit(f"{r['file']}: url {r['url']} is not an asset of {tag}")
extra = ["NOTICE.md", "LICENSE"]
for name in extra:
    if not (stage / name).is_file():
        sys.exit(f"{stage}: no {name}")
named = {r["file"] for r in rows} | {jsons[0].name} | set(extra)
stray = sorted(p.name for p in stage.iterdir() if p.name not in named)
if stray:
    sys.exit(f"{stage}: files the release does not name (weights? the spike's packages?): {stray}")

def check(d, label):
    bad = []
    for r in rows:
        p = d / r["file"]
        if not p.is_file():
            bad.append(f"{r['file']}: missing")
        elif p.stat().st_size != r["size"]:
            bad.append(f"{r['file']}: {p.stat().st_size} bytes, the manifest says {r['size']}")
        elif sha256(p) != r["sha256"]:
            bad.append(f"{r['file']}: sha256 differs from the manifest")
    if bad:
        sys.exit(f"{label}: " + "; ".join(bad))
    print(f"{label}: {len(rows)} packages match {jsons[0].name} (size and sha256)")

if mode == "check":
    check(stage, str(stage))
    assets = [jsons[0].name, *extra, *sorted(r["file"] for r in rows)]
    print("\nnot run by this script; the owner publishes once:\n")
    print(f"  cd {shlex.quote(str(stage))} && gh release create --prerelease --repo {repo} {tag} \\\n"
          f"    --title {tag} --notes-file NOTICE.md \\\n    " + " \\\n    ".join(shlex.quote(a) for a in assets))
    print(f"\nthen: {shlex.quote(me)} --verify-upload {shlex.quote(str(stage))}")
else:
    with tempfile.TemporaryDirectory(prefix=f"{tag}-check-") as t:
        for r in rows:
            subprocess.run(["gh", "release", "download", tag, "--repo", repo, "--dir", t,
                            "--pattern", r["file"]], check=True)
        check(Path(t), f"{repo} {tag} (downloaded)")
EOF
