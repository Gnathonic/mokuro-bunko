#!/usr/bin/env bash
# Check a staged detector-torch-v1 (the AnimeText packages) and print the command that publishes it.
#
#   tools/torch_export/publish_detector.sh <stage-dir>                  check, then print the command
#   tools/torch_export/publish_detector.sh --verify-upload <stage-dir>  after the upload: every
#                                                                       asset's sha256 and size from
#                                                                       `gh release view --json assets`
#
# <stage-dir> is what `python -m torch_export textbox-manifest -e animetext-blocks --tag
# detector-torch-v1 --licence AGPL-3.0 --flat <stage-dir>` writes (the packages under their flat
# release names and detector-torch-v1.json), plus NOTICE.md and the AGPL-3.0 text as LICENSE.
#
# In order, it refuses:
#   1. any package with its weights inside (`python -m torch_export refuse-embedded`, run first);
#   2. a missing NOTICE.md, or a LICENSE that is not the AGPL-3.0;
#   3. a stage that is not exactly the manifest's packages (size and sha256), its json, NOTICE.md
#      and LICENSE; a row that is not an animetext-blocks package under AGPL-3.0;
#   4. packages built from different exporter commits, or from a modified tree
#      (`bunko.exporter_commit` in each package), or a NOTICE.md that does not name that commit
#      and the ultralytics version the packages say they were built with;
#   5. an exporter commit that is not public yet: `git fetch` then `git branch -r --contains
#      <commit>` must list a branch (the export script must be public before the AGPL
#      packages are, owner answer O1).
#
# Nothing is uploaded by this script: the owner runs the printed `gh release create --prerelease`
# once, after every gate of plan 04 has passed, then `--verify-upload`.
# Environment: PYTHON (default: python3, with torch_export's requirements), GH_REPO (default:
# Gnathonic/mokuro-bunko), GIT_REMOTE (default: origin), PUBLISH_NO_FETCH=1 (skip `git fetch`:
# check against the remote branches already fetched).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
py="${PYTHON:-python3}"
repo="${GH_REPO:-Gnathonic/mokuro-bunko}"
remote="${GIT_REMOTE:-origin}"

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

# 2-4. the stage, its manifest and the exporter commit the packages carry
commit="$("$py" - "$stage" "$mode" <<'EOF'
import hashlib, json, sys, zipfile
from pathlib import Path

stage, mode = Path(sys.argv[1]), sys.argv[2]

def sha256(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        while b := f.read(1 << 24):
            h.update(b)
    return h.hexdigest()

def fail(msg):
    sys.exit(f"refused: {msg}")

for name in ("NOTICE.md", "LICENSE"):
    if not (stage / name).is_file():
        fail(f"{stage}: no {name}")
if "GNU AFFERO GENERAL PUBLIC LICENSE" not in (stage / "LICENSE").read_text(encoding="utf-8", errors="replace"):
    fail(f"{stage / 'LICENSE'}: not the AGPL-3.0 text")
jsons = sorted(stage.glob("*.json"))
if [p.name for p in jsons] != ["detector-torch-v1.json"]:
    fail(f"{stage}: expected detector-torch-v1.json alone, found {[p.name for p in jsons]}")
m = json.loads(jsons[0].read_text())
if m.get("release") != "detector-torch-v1":
    fail(f"{jsons[0]}: names release {m.get('release')!r}")
rows = m.get("files") or fail(f"{jsons[0]}: no files")
for r in rows:
    if r.get("engine") != "animetext-blocks" or r.get("kind") != "aoti" or not r["file"].endswith(".pt2"):
        fail(f"{r['file']}: not an animetext-blocks package; this release holds nothing else")
    if r.get("requires"):
        fail(f"{r['file']}: has requires; this release holds packages only")
    if r.get("licence") != "AGPL-3.0":
        fail(f"{r['file']}: licence {r.get('licence')!r}, not AGPL-3.0")
    if not r["url"].endswith(f"/detector-torch-v1/{r['file']}"):
        fail(f"{r['file']}: url {r['url']} is not an asset of detector-torch-v1")
named = {r["file"] for r in rows} | {jsons[0].name, "NOTICE.md", "LICENSE"}
stray = sorted(p.name for p in stage.iterdir() if p.name not in named)
if stray:
    fail(f"{stage}: files the release does not name (the ONNX file? the spike's packages?): {stray}")

commits, ultra = set(), set()
for r in rows:
    p = stage / r["file"]
    if not p.is_file():
        fail(f"{r['file']}: missing")
    if p.stat().st_size != r["size"] or sha256(p) != r["sha256"]:
        fail(f"{r['file']}: size or sha256 differs from the manifest")
    with zipfile.ZipFile(p) as z:
        metas = [n for n in z.namelist() if n.endswith(".wrapper_metadata.json")]
        if len(metas) != 1:
            fail(f"{r['file']}: {len(metas)} metadata files")
        meta = json.loads(z.read(metas[0]))
    commits.add(meta.get("bunko.exporter_commit", ""))
    ultra.add(meta.get("bunko.ultralytics", ""))
    if meta.get("bunko.io") != "detect-1" or meta.get("bunko.weights_format") != "onnx":
        fail(f"{r['file']}: not a detect-1 package bound to the ONNX file")
print(f"{stage}: {len(rows)} packages match {jsons[0].name} (size and sha256)", file=sys.stderr)
if len(commits) != 1 or len(ultra) != 1:
    fail(f"the packages come from {len(commits)} exporter commits {sorted(commits)} and ultralytics {sorted(ultra)}; "
         "one release is built from one exporter commit")
commit, version = commits.pop(), ultra.pop()
if not commit or commit == "unknown" or commit.endswith("-dirty"):
    fail(f"exporter commit {commit!r}: the packages were built from a modified or unknown tree")
notice = (stage / "NOTICE.md").read_text(encoding="utf-8")
for want in (commit, f"ultralytics {version}", "detector.py"):
    if want not in notice:
        fail(f"NOTICE.md does not name {want!r}")
print(commit)
EOF
)" || exit 1

# 5. the export script's commit must be public before the packages are
if [[ "${PUBLISH_NO_FETCH:-0}" != "1" ]]; then
  git -C "$here" fetch --quiet "$remote"
fi
if [[ -z "$(git -C "$here" branch -r --contains "$commit" 2>/dev/null)" ]]; then
  echo "refused: the export script's commit is not public yet ($commit is on no branch of $remote);" \
       "push it before detector-torch-v1 is published" >&2
  exit 1
fi

if [[ "$mode" == "check" ]]; then
  "$py" - "$stage" "$repo" "$0" <<'EOF'
import json, shlex, sys
from pathlib import Path

stage, repo, me = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
m = json.loads((stage / "detector-torch-v1.json").read_text())
assets = ["detector-torch-v1.json", "NOTICE.md", "LICENSE", *sorted(r["file"] for r in m["files"])]
print("\nnot run by this script; the owner publishes once, after every gate of plan 04 has passed:\n")
print(f"  cd {shlex.quote(str(stage))} && gh release create --prerelease --repo {repo} detector-torch-v1 \\\n"
      f"    --title detector-torch-v1 --notes-file NOTICE.md \\\n    " + " \\\n    ".join(shlex.quote(a) for a in assets))
print(f"\nthen: {shlex.quote(me)} --verify-upload {shlex.quote(str(stage))}")
EOF
else
  gh release view detector-torch-v1 --repo "$repo" --json isPrerelease,assets |
    "$py" - "$stage" <<'EOF'
import hashlib, json, sys
from pathlib import Path

stage = Path(sys.argv[1])
rel = json.load(sys.stdin)
m = json.loads((stage / "detector-torch-v1.json").read_text())

def sha256(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        while b := f.read(1 << 24):
            h.update(b)
    return h.hexdigest()

want = {r["file"]: (r["size"], r["sha256"]) for r in m["files"]}
for name in ("detector-torch-v1.json", "NOTICE.md", "LICENSE"):
    p = stage / name
    want[name] = (p.stat().st_size, sha256(p))
got = {a["name"]: (a.get("size"), (a.get("digest") or "").removeprefix("sha256:")) for a in rel.get("assets", [])}
bad = []
if not rel.get("isPrerelease"):
    bad.append("detector-torch-v1 is not a pre-release")
for name, (size, digest) in sorted(want.items()):
    if name not in got:
        bad.append(f"{name}: not uploaded")
    elif got[name] != (size, digest):
        bad.append(f"{name}: uploaded {got[name][0]} B sha256 {got[name][1] or '(none)'}, staged {size} B {digest}")
extra = sorted(set(got) - set(want))
if extra:
    bad.append(f"assets the stage does not hold: {extra}")
if bad:
    sys.exit("detector-torch-v1 (uploaded): " + "; ".join(bad))
print(f"detector-torch-v1 (uploaded): {len(want)} assets match the stage (size and sha256)")
EOF
fi
